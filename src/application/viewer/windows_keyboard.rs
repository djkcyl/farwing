//! Foreground keyboard routing. The hook never waits for transport or logs keys.
use std::collections::{BTreeMap, VecDeque};
use std::sync::{Mutex, MutexGuard, OnceLock, mpsc};
use std::thread::JoinHandle;

use anyhow::{Context, Result};
use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::SystemInformation::GetTickCount64;
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::Input::KeyboardAndMouse::{GetAsyncKeyState, GetKeyState};
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, DispatchMessageW, GetForegroundWindow, GetMessageW, KBDLLHOOKSTRUCT, MSG,
    PM_NOREMOVE, PeekMessageW, PostThreadMessageW, SetWindowsHookExW, TranslateMessage,
    UnhookWindowsHookEx, WH_KEYBOARD_LL, WM_KEYDOWN, WM_KEYUP, WM_QUIT, WM_SYSKEYDOWN, WM_SYSKEYUP,
};

use crate::features::remote_input::{MouseMode, RemoteInput};

/// Winit registers mouse AND keyboard raw devices at event-loop creation.
/// This client uses only RAWMOUSE; keyboard uses a hook plus window state events.
/// Remove just the unused keyboard class before showing/focusing a window.
pub(super) fn remove_unused_raw_keyboard() -> Result<()> {
    use windows::Win32::UI::Input::{RAWINPUTDEVICE, RIDEV_REMOVE, RegisterRawInputDevices};
    let keyboard = RAWINPUTDEVICE {
        usUsagePage: 1,
        usUsage: 6,
        dwFlags: RIDEV_REMOVE,
        hwndTarget: HWND::default(),
    };
    unsafe { RegisterRawInputDevices(&[keyboard], std::mem::size_of::<RAWINPUTDEVICE>() as u32) }
        .context("移除未使用的 Raw 键盘注册失败")?;
    tracing::debug!("unused raw keyboard registration removed; raw mouse retained");
    Ok(())
}

struct Target {
    owner: u64,
    input: RemoteInput,
    generation: u64,
    activation: u64,
    intercept_shortcuts: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Route {
    Local,
    Window(u64),
    Hook(u64),
}

/// Latest native observation. GUI snapshots/dispatch never rewrite this table.
/// Scope identifies the acquisition of control, not merely a reusable HWND.
#[derive(Clone, Copy)]
struct NativeKey {
    down: bool,
    route: Route,
    scope: u64,
    time: u32,
    seen: bool,
    window_owner: Option<u64>,
    own: bool,
}
impl Default for NativeKey {
    fn default() -> Self {
        Self {
            down: false,
            route: Route::Local,
            scope: 0,
            time: 0,
            seen: false,
            window_owner: None,
            own: false,
        }
    }
}
struct NativeKeys([NativeKey; 256]);
impl Default for NativeKeys {
    fn default() -> Self {
        Self([NativeKey::default(); 256])
    }
}
impl NativeKeys {
    fn held(&self) -> [bool; 256] {
        self.0.map(|key| key.down)
    }
    fn older(&self, edge: KeyEdge) -> bool {
        let key = self.0[usize::from(edge.key)];
        key.seen && (edge.time.wrapping_sub(key.time) as i32) < 0
    }
    fn observe(&mut self, edge: KeyEdge, route: Route, scope: u64) {
        self.0[usize::from(edge.key)] = NativeKey {
            down: edge.down,
            route: if edge.down { route } else { Route::Local },
            scope,
            time: edge.time,
            seen: true,
            own: edge.own,
            window_owner: match route {
                Route::Window(owner) => Some(owner),
                _ => None,
            },
        };
    }
}

#[derive(Clone, Copy)]
struct KeyEdge {
    key: u16,
    scan: u32,
    down: bool,
    injected: bool,
    time: u32,
    fresh: bool,
    coalesced: bool,
    own: bool,
}

struct PendingKey {
    edge: KeyEdge,
    route: Route,
    owner: u64,
    ready: bool,
    scope: u64,
}

const MAX_PENDING_KEYS: usize = 512;

struct Router {
    installed: bool,
    shortcut: Option<(u64, super::presenter::ViewerShortcut)>,
    target: Option<Target>,
    // Ordered consumer state, seeded on acquisition; not physical-device truth.
    dispatch_down: [bool; 256],
    // DOWN inherited from an activation snapshot, not from our event stream.
    sampled: [bool; 256],
    // An old intercepted ordinary key has no authoritative async state. Keep
    // it quarantined until UP, without making it a global activation barrier.
    isolated: [bool; 256],
    blocked: [bool; 256],
    consumed: [bool; 256],
    lock_releases: Vec<(u64, u16, u64)>,
    diagnostic_seen: u64,
    neutral_wait_since: std::time::Instant,
    configuration: u64,
    native: NativeKeys,
    scope: u64,
    scope_started: u32,
    pending: VecDeque<PendingKey>,
    windows: BTreeMap<u64, crate::features::stream_control::StreamControlHandle>,
}

impl Default for Router {
    fn default() -> Self {
        Self {
            installed: false,
            shortcut: None,
            target: None,
            dispatch_down: [false; 256],
            sampled: [false; 256],
            isolated: [false; 256],
            blocked: [false; 256],
            consumed: [false; 256],
            lock_releases: Vec::new(),
            diagnostic_seen: 0,
            neutral_wait_since: std::time::Instant::now(),
            configuration: crate::application::viewer_shortcuts::revision(),
            native: NativeKeys::default(),
            scope: 1,
            scope_started: unsafe { GetTickCount64() } as u32,
            pending: VecDeque::new(),
            windows: BTreeMap::new(),
        }
    }
}

fn router() -> MutexGuard<'static, Router> {
    static ROUTER: OnceLock<Mutex<Router>> = OnceLock::new();
    ROUTER
        .get_or_init(|| Mutex::new(Router::default()))
        .lock()
        .unwrap_or_else(|p| p.into_inner())
}

fn with_router<T>(f: impl FnOnce(&mut Router) -> T) -> T {
    f(&mut router())
}

fn blocked_modifiers(blocked: &[bool; 256]) -> bool {
    [91, 92, 160, 161, 162, 163, 164, 165]
        .into_iter()
        .any(|key| blocked[key])
}

fn activation_modifier(key: u16) -> bool {
    matches!(key, 16..=18 | 91..=92 | 160..=165)
}

fn windows_state_key(key: u16) -> bool {
    matches!(key, 16..=18 | 20 | 144..=145 | 160..=165)
}

fn keyboard_key(key: u16) -> bool {
    // Canonical keyboard VKs (left/right modifiers have already been resolved).
    // Do not wait for releases of reserved, gamepad, IME process/packet or
    // intermediate values. Keep international, IME On/Off and OEM keyboard keys.
    matches!(key,
        0x08 | 0x09 | 0x0c | 0x0d | 0x13..=0x19 |
        0x1a..=0x39 | 0x41..=0x5d | 0x5f..=0x87 | 0x90..=0x96 |
        0xa0..=0xb7 | 0xba..=0xc0 | 0xdb..=0xdf | 0xe1..=0xe4 |
        0xe6 | 0xe9..=0xfb | 0xfd..=0xfe)
}

fn intercept_key(
    key: u16,
    held: &[bool; 256],
    input: &RemoteInput,
    intercept_shortcuts: bool,
) -> bool {
    // Capture ordinary keys before local RegisterHotKey/IME handling can take
    // them away from the viewer. A list of known system chords misses arbitrary
    // application hotkeys (including single-key bindings such as PrintScreen).
    // Like official UU, let Ctrl/Shift/Alt and lock keys update Windows state;
    // their window events remain ordered before the intercepted primary key.
    if crate::application::viewer_shortcuts::suspended() || windows_state_key(key) {
        return false;
    }
    if super::input_gate::intercepts_shortcuts(input, intercept_shortcuts) {
        return true;
    }
    let modifiers = u8::from(held[162] || held[163])
        | (u8::from(held[160] || held[161]) << 1)
        | (u8::from(held[164] || held[165]) << 2)
        | (u8::from(held[91] || held[92]) << 3);
    crate::application::viewer_shortcuts::match_key(key, modifiers).is_some()
}

pub(super) struct SessionNotifications {
    owner: u64,
    registered: bool,
}

impl SessionNotifications {
    pub fn new(owner: u64, control: &crate::features::stream_control::StreamControlHandle) -> Self {
        use windows::Win32::System::RemoteDesktop::{
            NOTIFY_FOR_THIS_SESSION, WTSRegisterSessionNotification,
        };
        with_router(|r| {
            r.windows.insert(owner, control.clone());
        });
        let registered =
            unsafe { WTSRegisterSessionNotification(HWND(owner as _), NOTIFY_FOR_THIS_SESSION) }
                .map_err(|error| tracing::warn!(%error, "session lock notification unavailable"))
                .is_ok();
        Self { owner, registered }
    }
}

impl Drop for SessionNotifications {
    fn drop(&mut self) {
        if self.registered {
            let _ = unsafe {
                windows::Win32::System::RemoteDesktop::WTSUnRegisterSessionNotification(HWND(
                    self.owner as _,
                ))
            };
        }
        with_router(|r| {
            r.windows.remove(&self.owner);
        });
    }
}

fn normalize_key(key: u32, scan: u32, extended: bool) -> u16 {
    match key {
        16 => {
            if scan == 0x36 {
                161
            } else {
                160
            }
        }
        17 => {
            if extended {
                163
            } else {
                162
            }
        }
        18 => {
            if extended {
                165
            } else {
                164
            }
        }
        _ => key as u16,
    }
}

/// Before TranslateMessage/winit/egui, on the window thread, not a render tick.
pub(super) fn message(pointer: *const std::ffi::c_void) -> bool {
    if pointer.is_null() {
        return false;
    }
    let msg = unsafe { &*pointer.cast::<MSG>() };
    let key_message = matches!(
        msg.message,
        WM_KEYDOWN | WM_KEYUP | WM_SYSKEYDOWN | WM_SYSKEYUP
    );
    with_router(|r| {
        if msg.message == windows::Win32::UI::WindowsAndMessaging::WM_WTSSESSION_CHANGE
            && matches!(msg.wParam.0, 2 | 4 | 6 | 7) // console/remote disconnect, logoff, lock
            && r.windows.contains_key(&(msg.hwnd.0 as u64))
        {
            r.suspend_desktop();
            return false;
        }
        if !key_message {
            r.drain_keys();
            return false;
        }
        if !(8..=254).contains(&msg.wParam.0) || msg.wParam.0 == 231 {
            return false;
        }
        let own = crate::platform::windows::input::system::own_message();
        let flags = msg.lParam.0 as u32;
        let scan = (flags >> 16) & 0xff;
        let virtual_key = if msg.wParam.0 == 229 {
            // The local IME may replace wParam with VK_PROCESSKEY. Preserve
            // the original physical key; committed text is not forwarded.
            unsafe { windows::Win32::UI::Input::Ime::ImmGetVirtualKey(msg.hwnd) }
        } else {
            msg.wParam.0 as u32
        };
        if !(8..=254).contains(&virtual_key) || virtual_key == 231 {
            return false;
        }
        let down = matches!(msg.message, WM_KEYDOWN | WM_SYSKEYDOWN);
        let mut key = normalize_key(virtual_key, scan, flags & (1 << 24) != 0);
        if scan == 0 && matches!(virtual_key, 16..=18) {
            // SendInput without a scan code can leave a generic modifier in
            // WM_KEY*. Its associated hook edge still identifies the side.
            let left = 160 + 2 * (virtual_key as u16 - 16);
            if let Some(pending) = r.pending.iter().find(|p| {
                p.owner == msg.hwnd.0 as u64
                    && p.scope == r.scope
                    && !p.ready
                    && p.edge.time == msg.time
                    && p.edge.down == down
                    && (p.edge.key == left || p.edge.key == left + 1)
            }) {
                key = pending.edge.key;
            }
        }
        if !keyboard_key(key) {
            return false;
        }
        let count = if down { (flags & 0xffff).max(1) } else { 1 };
        if count > MAX_PENDING_KEYS as u32 {
            r.stop_ordering();
            return true;
        }
        let mut consumed = false;
        for repeat in 0..count {
            consumed |= r.window_edge(
                msg.hwnd.0 as u64,
                KeyEdge {
                    key,
                    scan,
                    down,
                    injected: false,
                    time: msg.time,
                    fresh: down && flags & (1 << 30) == 0 && repeat == 0,
                    coalesced: count > 1,
                    own,
                },
            );
        }
        consumed
    })
}

pub(super) struct KeyboardHook {
    thread: Option<JoinHandle<()>>,
    thread_id: u32,
}

impl KeyboardHook {
    pub fn install() -> Result<Self> {
        let (ready, receive) = mpsc::sync_channel(1);
        let thread = std::thread::Builder::new()
            .name("keyboard-input".into())
            .spawn(move || {
                // The installing thread must keep pumping messages. Do not tie
                // LowLevelHooksTimeout to GUI/font/device initialization or drawing.
                let mut message = MSG::default();
                unsafe {
                    let _ = PeekMessageW(&mut message, None, 0, 0, PM_NOREMOVE);
                }
                let result = (|| -> Result<_> {
                    let module =
                        unsafe { GetModuleHandleW(None) }.context("取得键盘控制模块失败")?;
                    unsafe {
                        SetWindowsHookExW(
                            WH_KEYBOARD_LL,
                            Some(keyboard_proc),
                            Some(HINSTANCE(module.0)),
                            0,
                        )
                    }
                    .context("安装键盘控制钩子失败")
                })();
                let hook = match result {
                    Ok(hook) => hook,
                    Err(error) => {
                        let _ = ready.send(Err(error));
                        return;
                    }
                };
                router().installed = true;
                tracing::debug!("keyboard hook installed on dedicated message pump");
                let thread_id = unsafe { GetCurrentThreadId() };
                if ready.send(Ok(thread_id)).is_ok() {
                    loop {
                        let result = unsafe { GetMessageW(&mut message, None, 0, 0) }.0;
                        if result <= 0 {
                            break;
                        }
                        unsafe {
                            let _ = TranslateMessage(&message);
                            DispatchMessageW(&message);
                        }
                    }
                }
                let _ = unsafe { UnhookWindowsHookEx(hook) };
                let mut r = router();
                r.clear();
                r.installed = false;
            })
            .context("创建键盘输入线程失败")?;
        match receive
            .recv()
            .context("键盘输入线程启动失败")
            .and_then(|result| result)
        {
            Ok(thread_id) => Ok(Self {
                thread: Some(thread),
                thread_id,
            }),
            Err(error) => {
                let _ = thread.join();
                Err(error)
            }
        }
    }
}

impl Drop for KeyboardHook {
    fn drop(&mut self) {
        {
            let mut r = router();
            r.clear();
            r.installed = false;
        }
        if let Some(thread) = self.thread.take() {
            if unsafe { PostThreadMessageW(self.thread_id, WM_QUIT, WPARAM(0), LPARAM(0)) }.is_ok()
                || thread.is_finished()
            {
                let _ = thread.join();
            } else {
                tracing::warn!("could not stop keyboard message pump; routing is disabled");
            }
        }
    }
}

impl Router {
    fn snapshot_keys(&mut self, mut down: impl FnMut(u16) -> bool) -> (usize, usize) {
        let mut ignored = 0;
        let mut intercepted = 0;
        for i in 8..255 {
            let canonical = keyboard_key(i as u16);
            let sampled = down(i as u16);
            ignored += usize::from(sampled && !canonical);
            // An intercepted DOWN may never enter Windows' async state. Its
            // live hook route, not a stale local-consumption obligation, owns it.
            let hook_held = canonical
                && self.native.0[i].down
                && matches!(self.native.0[i].route, Route::Hook(_));
            intercepted += usize::from(hook_held);
            self.dispatch_down[i] = canonical && (sampled || hook_held);
            self.sampled[i] = canonical && sampled && !hook_held;
            self.isolated[i] = hook_held && !sampled && !activation_modifier(i as u16);
            self.blocked[i] = self.dispatch_down[i];
        }
        for i in [1, 2, 4, 5, 6] {
            self.dispatch_down[i] = down(i as u16);
            self.sampled[i] = self.dispatch_down[i];
        }
        let isolated = self.isolated.iter().filter(|held| **held).count();
        if isolated != 0 {
            tracing::debug!(
                isolated_keys = isolated,
                "inherited hook keys quarantined separately from activation"
            );
        }
        (ignored, intercepted)
    }

    fn reconcile_sampled_keys(&mut self, mut down: impl FnMut(u16) -> bool) {
        // Called on the foreground GUI thread, never from the low-level hook
        // (whose callback precedes the async-state update). Do not overtake
        // ordered key edges, synthesize input, or clear a hook-owned hold.
        if !self.pending.is_empty()
            || !self
                .target
                .as_ref()
                .is_some_and(|t| t.input.waiting_for_neutral())
        {
            return;
        }
        let mut released = 0;
        for i in 0..256 {
            if self.sampled[i] && !down(i as u16) {
                self.sampled[i] = false;
                self.dispatch_down[i] = false;
                self.blocked[i] = false;
                released += 1;
            }
        }
        if released != 0 {
            tracing::debug!(
                released_count = released,
                "activation input snapshot releases reconciled"
            );
        }
        self.finish_neutral();
    }

    /// Return whether this edge belonged to the blocked activation, including
    /// the final UP. Only a later edge may be sent after both devices are neutral.
    fn finish_neutral(&self) -> bool {
        let Some(target) = &self.target else {
            return false;
        };
        if !target.input.waiting_for_neutral() {
            return false;
        }
        if !self
            .dispatch_down
            .iter()
            .zip(self.isolated)
            .any(|(down, isolated)| *down && !isolated)
        {
            target.input.confirm_neutral(target.activation);
            if !target.input.waiting_for_neutral() {
                tracing::info!(
                    elapsed_ms = self.neutral_wait_since.elapsed().as_millis() as u64,
                    "keyboard and mouse activation wait cleared"
                );
            }
        }
        true
    }

    fn suspend_desktop(&mut self) {
        // This is a control revocation, not a synthetic remote lock command.
        // Release obligations already handed to transport remain in its queue.
        for control in self.windows.values() {
            let state = control.snapshot();
            if state.mouse_mode != MouseMode::View || state.mouse_pending {
                let _ = control.suspend_mouse_control();
            }
            control.mouse().reconcile_keyboard_releases();
        }
        self.clear();
        self.dispatch_down = [false; 256];
        self.sampled = [false; 256];
        self.isolated = [false; 256];
        self.native = NativeKeys::default();
        self.blocked = [false; 256];
        self.consumed = [false; 256];
        tracing::debug!("local desktop transition revoked remote input");
    }
    // One observation per stage/source/activation. No per-event keys or text.
    fn diagnostic(&mut self, stage: u8, injected: bool, reason: &'static str) {
        if self.target.is_none() {
            return;
        }
        let bit = 1u64 << (stage * 2 + u8::from(injected));
        if self.diagnostic_seen & bit == 0 {
            self.diagnostic_seen |= bit;
            tracing::debug!(injected, reason, "keyboard routing observation");
        }
    }
    fn clear(&mut self) {
        self.scope = self.scope.wrapping_add(1);
        self.scope_started = unsafe { GetTickCount64() } as u32;
        self.shortcut = None;
        self.lock_releases.clear();
        self.pending.clear();
        if let Some(target) = self.target.take() {
            target.input.pause_owner(target.owner);
        }
        for (blocked, down) in self.blocked.iter_mut().zip(self.dispatch_down) {
            *blocked |= down;
        }
    }

    fn stop_ordering(&mut self) {
        if let Some(target) = &self.target {
            target.input.pause_for_recovery("键盘事件顺序中断");
        }
        self.clear();
    }

    fn push_edge(&mut self, edge: KeyEdge, route: Route) {
        if self.pending.len() >= MAX_PENDING_KEYS {
            self.stop_ordering();
            return;
        }
        self.pending.push_back(PendingKey {
            edge,
            route,
            owner: match route {
                Route::Window(owner) | Route::Hook(owner) => owner,
                Route::Local => self.target.as_ref().map_or(0, |t| t.owner),
            },
            ready: matches!(route, Route::Hook(_)),
            scope: self.scope,
        });
    }

    fn drain_keys(&mut self) {
        if self
            .target
            .as_ref()
            .is_some_and(|t| t.activation != t.input.activation_generation())
        {
            // Transport/UI reactivation can precede set_target on the next
            // frame. Queued edges still belong to the previous acquisition.
            self.clear();
            return;
        }
        while self.pending.front().is_some_and(|key| key.ready) {
            let key = self.pending.pop_front().unwrap();
            if key.scope != self.scope {
                continue;
            }
            let edge = key.edge;
            if key.route == Route::Local {
                crate::plugins::hotkeys::key_event(0, edge.key, edge.down);
                let i = usize::from(edge.key);
                self.dispatch_down[i] = edge.down;
                self.sampled[i] = false;
                self.blocked[i] = edge.down;
                if !edge.down {
                    self.consumed[i] = false;
                    self.isolated[i] = false;
                }
                self.finish_neutral();
            } else {
                self.event(edge.key, edge.scan, edge.down, edge.injected);
            }
        }
    }

    fn observe_hook(&mut self, edge: KeyEdge) -> bool {
        if self.filter_own_input(edge) {
            return false;
        }
        let i = usize::from(edge.key);
        self.sampled[i] = false;
        let previous = self.native.0[i];
        let continuing = previous.down
            && (previous.scope == self.scope
                || matches!(previous.route, Route::Hook(_))
                || self.dispatch_down[i]);
        let (route, scope) = if edge.down && !continuing {
            let owner = self
                .target
                .as_ref()
                .filter(|t| {
                    self.installed
                        && unsafe { GetForegroundWindow() } == HWND(t.owner as _)
                        && t.input.mode() != MouseMode::View
                        && super::windows_mouse::router().keyboard_allowed(t.owner)
                })
                .map(|t| t.owner);
            let mut held = self.native.held();
            for key in [91, 92, 160, 161, 162, 163, 164, 165] {
                if self.sampled[key] {
                    held[key] = self.dispatch_down[key];
                }
            }
            held[i] = edge.down;
            let route = if let Some(owner) = owner {
                if self.target.as_ref().is_some_and(|t| {
                    t.input.keyboard_supported()
                        && intercept_key(edge.key, &held, &t.input, t.intercept_shortcuts)
                }) {
                    Route::Hook(owner)
                } else {
                    Route::Window(owner)
                }
            } else {
                Route::Local
            };
            (route, self.scope)
        } else {
            (previous.route, previous.scope)
        };
        self.native.observe(edge, route, scope);
        let current = scope == self.scope
            && match route {
                Route::Local => self
                    .target
                    .as_ref()
                    .is_some_and(|t| unsafe { GetForegroundWindow() } == HWND(t.owner as _)),
                Route::Window(owner) | Route::Hook(owner) => {
                    self.target.as_ref().is_some_and(|t| t.owner == owner)
                }
            };
        if current {
            self.push_edge(edge, route);
            if let Route::Hook(owner) = route {
                super::windows_mouse::router().wake_keyboard(owner);
            }
        } else if !edge.down {
            // Observed release retires the old press even if its GUI owner was
            // revoked. It must never be dispatched into the new acquisition.
            self.retire_key(i);
            self.finish_neutral();
        }
        matches!(route, Route::Hook(_))
    }

    fn window_edge(&mut self, owner: u64, edge: KeyEdge) -> bool {
        if self.filter_own_input(edge) {
            return false;
        }
        let i = usize::from(edge.key);
        let position = self.pending.iter().position(|p| {
            p.owner == owner
                && !matches!(p.route, Route::Hook(_))
                && !p.ready
                && p.edge.key == edge.key
                && p.edge.down == edge.down
                && (p.edge.time == edge.time || edge.coalesced)
                && p.scope == self.scope
        });
        let mut owned = self.native.0[i].window_owner == Some(owner);
        if let Some(mut position) = position {
            // A missing earlier window event cannot be bypassed by a later
            // intercepted key, except an unclaimed ordinary key: interception
            // is off (or was suspended) and a local hotkey/IME may consume it.
            // A later window key message proves such a marker can be retired;
            // don't guess from WM_NULL, which can precede queued input.
            if self
                .pending
                .iter()
                .take(position)
                .any(|p| !p.ready && (p.route == Route::Local || windows_state_key(p.edge.key)))
            {
                self.stop_ordering();
            } else {
                for previous in (0..position).rev() {
                    if !self.pending[previous].ready {
                        self.pending.remove(previous);
                        position -= 1;
                    }
                }
                let injected = self.pending[position].edge.injected;
                self.pending[position].edge = KeyEdge { injected, ..edge };
                self.pending[position].ready = true;
                owned = matches!(self.pending[position].route, Route::Window(_));
                self.drain_keys();
            }
        } else if self.native.older(edge) || (edge.time.wrapping_sub(self.scope_started) as i32) < 0
        {
            // A delayed window message must not overwrite a later hook edge.
            return owned || matches!(self.native.0[i].route, Route::Hook(id) if id == owner);
        } else if self.target.as_ref().is_some_and(|t| t.owner == owner) {
            // Window input continues to work even if the hook wasn't called.
            // No timer-based duplicate detection or duplicate fallback sends.
            if self
                .pending
                .iter()
                .any(|p| !p.ready || p.edge.key == edge.key)
            {
                // An unmatched window edge cannot be ordered against a queued
                // hook edge of the same key. Revoke rather than release a newer
                // press, or replay an old press after its release.
                self.stop_ordering();
            } else {
                self.drain_keys();
                if !edge.down {
                    owned |= self.release_window_route(owner, edge);
                }
                let event_scope = self.scope;
                if edge.down && edge.fresh {
                    // WM_KEYDOWN bit 30 proves this is a new press, not an
                    // inherited repeat. Retire an unbalanced old press first.
                    if let Some(target) = &self.target {
                        target.input.key(owner, edge.key, false, None);
                    }
                    self.retire_key(i);
                }
                owned |= self.event(edge.key, edge.scan, edge.down, edge.injected);
                if edge.down
                    && self.target.as_ref().is_some_and(|t| {
                        t.input.owner_holds_key(owner, edge.key) || self.consumed[i]
                    })
                {
                    self.native.observe(edge, Route::Window(owner), event_scope);
                } else {
                    // Observation is independent of permission to forward. A
                    // rejected native edge still supersedes an older record.
                    self.native.observe(edge, Route::Local, event_scope);
                }
            }
        } else if !edge.down && !self.pending.iter().any(|p| p.edge.key == edge.key) {
            // UP can arrive after the viewer lost focus or closed its target.
            // Retire only that window's old route, never another owner's hold.
            owned |= self.release_window_route(owner, edge);
        }
        if !edge.down {
            if !self.native.0[i].down && !self.native.older(edge) {
                self.native.0[i].window_owner = None;
            }
        }
        owned && !matches!(edge.key, 16..=18 | 20 | 144..=145 | 160..=165)
    }

    fn filter_own_input(&mut self, edge: KeyEdge) -> bool {
        if !edge.own
            || self
                .target
                .as_ref()
                .is_some_and(|t| t.input.accepts_host_input())
        {
            return false;
        }
        let i = usize::from(edge.key);
        let previous = self.native.0[i];
        if !edge.down && previous.own && previous.down && !self.native.older(edge) {
            // Permission can change between an accepted injected DOWN and UP.
            // Reject new injection, but keep the old release obligation intact.
            if previous.scope == self.scope {
                if let Some(target) = &self.target {
                    target.input.key(target.owner, edge.key, false, None);
                }
            }
            self.native.observe(edge, previous.route, previous.scope);
            self.retire_key(i);
            self.finish_neutral();
        }
        true
    }

    fn retire_key(&mut self, i: usize) {
        self.dispatch_down[i] = false;
        self.sampled[i] = false;
        self.isolated[i] = false;
        self.blocked[i] = false;
        self.consumed[i] = false;
    }

    fn release_window_route(&mut self, owner: u64, edge: KeyEdge) -> bool {
        let i = usize::from(edge.key);
        let route = self.native.0[i].route;
        if self.native.older(edge)
            || !matches!(route, Route::Hook(id) | Route::Window(id) if id == owner || self.native.0[i].scope != self.scope)
        {
            return false;
        }
        self.native.observe(edge, route, self.native.0[i].scope);
        self.retire_key(i);
        matches!(route, Route::Hook(_))
    }

    fn modifiers(&self) -> (bool, bool, bool, bool) {
        let p = &self.dispatch_down;
        (
            p[17] || p[162] || p[163],
            p[16] || p[160] || p[161],
            p[18] || p[164] || p[165],
            p[91] || p[92],
        )
    }

    fn event(&mut self, vk: u16, _scan: u32, down: bool, injected: bool) -> bool {
        self.diagnostic(0, injected, "window_dispatch_received");
        let index = usize::from(vk);
        // VK_PACKET is committed Unicode input, not a physical VK event.
        if !keyboard_key(vk) {
            self.diagnostic(1, injected, "not_physical_key");
            return false;
        }
        let previously_down = self.dispatch_down[index];
        let was_isolated = self.isolated[index];
        self.dispatch_down[index] = down;
        self.sampled[index] = false;
        let was_consumed = self.consumed[index];
        if !down {
            self.consumed[index] = false;
            self.isolated[index] = false;
        }
        if was_isolated {
            // Repeats inherited from the previous owner must not execute a
            // newly bound local shortcut/plugin either. UP only ends quarantine.
            self.blocked[index] = down;
            self.finish_neutral();
            return was_consumed;
        }
        let Some(target) = &mut self.target else {
            if !down {
                self.blocked[index] = false;
            }
            return was_consumed;
        };
        if unsafe { GetForegroundWindow() } != HWND(target.owner as _)
            || target.input.mode() == MouseMode::View
        {
            self.diagnostic(2, injected, "inactive_owner");
            self.clear();
            if !down {
                self.blocked[index] = false;
            }
            return was_consumed;
        }
        let configuration = crate::application::viewer_shortcuts::revision();
        if configuration != self.configuration || crate::application::viewer_shortcuts::suspended()
        {
            self.configuration = configuration;
            target.input.pause_owner(target.owner);
            target.generation = target.input.keyboard_generation();
            for (blocked, physical) in self.blocked.iter_mut().zip(self.dispatch_down) {
                *blocked |= physical;
            }
            if !previously_down {
                self.blocked[index] = false;
            }
            if crate::application::viewer_shortcuts::suspended() {
                self.blocked[index] = down;
                return was_consumed;
            }
        }
        let generation = target.input.keyboard_generation();
        if generation != target.generation {
            target.generation = generation;
            for (blocked, physical) in self.blocked.iter_mut().zip(self.dispatch_down) {
                *blocked |= physical;
            }
            // This new edge belongs to the resumed owner, not to the old
            // generation. Only keys already physically held are quarantined.
            if !previously_down {
                self.blocked[index] = false;
            }
        }
        let (ctrl, shift, alt, win) = self.modifiers();
        if let Some(action) = crate::application::viewer_shortcuts::match_key(
            vk,
            u8::from(ctrl) | (u8::from(shift) << 1) | (u8::from(alt) << 2) | (u8::from(win) << 3),
        ) {
            // Forwarded keys are swallowed; use native modifier state rather
            // than reconstructing the chord from winit's partial key stream.
            if down {
                if let Some(t) = &mut self.target {
                    t.input.pause_owner(t.owner);
                    t.generation = t.input.keyboard_generation();
                    if !previously_down {
                        self.shortcut = Some((t.owner, action));
                        super::windows_mouse::router().wake_keyboard(t.owner);
                    }
                }
                for (blocked, physical) in self.blocked.iter_mut().zip(self.dispatch_down) {
                    *blocked |= physical;
                }
                self.consumed[index] = true;
            } else {
                self.blocked[index] = false;
            }
            return true;
        }
        if let Some(target) = &self.target {
            if crate::plugins::hotkeys::key_event(target.owner, vk, down) {
                self.consumed[index] = down;
                return true;
            }
        }
        if self.finish_neutral() {
            self.blocked[index] = down;
            self.diagnostic(8, injected, "waiting_for_keyboard_mouse_neutral");
            return was_consumed;
        }
        if self.blocked[index] {
            self.diagnostic(3, injected, "quarantined_key");
            if !down {
                self.blocked[index] = false;
            }
            return was_consumed;
        }
        let target = self.target.as_mut().expect("checked target");
        let allowed = super::windows_mouse::router().keyboard_allowed(target.owner);
        if down && (!allowed || blocked_modifiers(&self.blocked)) {
            self.diagnostic(
                if allowed { 4 } else { 5 },
                injected,
                if allowed {
                    "quarantined_modifier"
                } else {
                    "outside_input_area"
                },
            );
            self.blocked[index] = true;
            return was_consumed;
        }
        let target = self.target.as_mut().expect("checked target");
        if !down && matches!(vk, 20 | 144 | 145) {
            // Toggle state belongs to the foreground GUI input queue. The
            // dedicated hook thread must not sample its own GetKeyState table.
            self.lock_releases.retain(|(_, key, _)| *key != vk);
            self.lock_releases
                .push((target.owner, vk, target.generation));
            super::windows_mouse::router().wake_keyboard(target.owner);
            return false;
        }
        let lock = matches!(vk, 20 | 144 | 145).then_some(0);
        let accepted = target.input.key(target.owner, vk, down, lock);
        target.generation = target.input.keyboard_generation();
        self.diagnostic(
            if accepted { 6 } else { 7 },
            injected,
            if accepted { "queued" } else { "queue_rejected" },
        );
        // Let Windows maintain modifier/lock state, including AltGr's injected
        // Ctrl. Ordinary accepted input must not also reach egui/local IME.
        let consume = accepted && !matches!(vk, 16..=18 | 20 | 144..=145 | 160..=165);
        if down && consume {
            self.consumed[index] = true;
        }
        consume || was_consumed
    }
}

pub(super) fn set_target(owner: u64, input: &RemoteInput, intercept_shortcuts: bool) {
    with_router(|r| {
        let activation = input.activation_generation();
        if r.target.as_ref().is_some_and(|t| {
            t.owner == owner
                && t.activation == activation
                && t.input.same_session(input)
                && t.intercept_shortcuts == intercept_shortcuts
        }) {
            if unsafe { GetForegroundWindow() } == HWND(owner as _) {
                r.reconcile_sampled_keys(|key| unsafe { GetAsyncKeyState(i32::from(key)) } < 0);
            }
            return;
        }
        r.clear();
        r.diagnostic_seen = 0;
        r.neutral_wait_since = std::time::Instant::now();
        // Keys held before activation belong to the previous local surface.
        let (ignored_snapshot_states, intercepted_keys) =
            r.snapshot_keys(|key| unsafe { GetAsyncKeyState(i32::from(key)) } < 0);
        tracing::debug!(
            quarantined_keys = (8..255).filter(|i| r.blocked[*i]).count(),
            held_modifiers = blocked_modifiers(&r.blocked),
            ignored_snapshot_states,
            intercepted_keys,
            held_mouse_buttons = [1, 2, 4, 5, 6]
                .into_iter()
                .filter(|i| r.dispatch_down[*i])
                .count(),
            "keyboard input owner activated"
        );
        r.target = Some(Target {
            owner,
            input: input.clone(),
            generation: input.keyboard_generation(),
            activation,
            intercept_shortcuts,
        });
        r.finish_neutral();
    });
}

pub(super) fn observe_mouse_buttons(owner: u64, flags: u16) -> bool {
    with_router(|r| {
        if r.target.as_ref().is_none_or(|t| t.owner != owner) {
            return false;
        }
        for (index, key) in [1, 2, 4, 5, 6].into_iter().enumerate() {
            if flags & (1 << (2 * index)) != 0 {
                r.dispatch_down[key] = true;
                r.sampled[key] = false;
            }
            if flags & (2 << (2 * index)) != 0 {
                r.dispatch_down[key] = false;
                r.sampled[key] = false;
            }
        }
        r.finish_neutral()
    })
}

pub(super) fn clear(owner: u64) {
    with_router(|r| {
        if r.target.as_ref().is_some_and(|t| t.owner == owner) {
            tracing::debug!("keyboard input owner deactivated by GUI");
            r.clear();
        }
    });
}

unsafe extern "system" fn keyboard_proc(code: i32, message: WPARAM, data: LPARAM) -> LRESULT {
    if code >= 0 && data.0 != 0 {
        let down = matches!(message.0 as u32, WM_KEYDOWN | WM_SYSKEYDOWN);
        if down || matches!(message.0 as u32, WM_KEYUP | WM_SYSKEYUP) {
            let event = unsafe { &*(data.0 as *const KBDLLHOOKSTRUCT) };
            let vk = u32::from(normalize_key(
                event.vkCode,
                event.scanCode,
                event.flags.0 & 1 != 0,
            ));
            let scan = if event.scanCode == 0 {
                use windows::Win32::UI::Input::KeyboardAndMouse::{
                    MAPVK_VK_TO_VSC, MapVirtualKeyW,
                };
                unsafe { MapVirtualKeyW(vk, MAPVK_VK_TO_VSC) }
            } else {
                event.scanCode
            };
            if keyboard_key(vk as u16)
                && with_router(|r| {
                    r.observe_hook(KeyEdge {
                        key: vk as u16,
                        scan,
                        down,
                        injected: event.flags.0 & 0x10 != 0,
                        time: event.time,
                        fresh: false,
                        coalesced: false,
                        own: event.dwExtraInfo
                            == crate::platform::windows::input::system::INPUT_MARKER,
                    })
                })
            {
                return LRESULT(1);
            }
        }
    }
    unsafe { CallNextHookEx(None, code, message, data) }
}

pub(super) fn take_shortcut(owner: u64) -> Option<super::presenter::ViewerShortcut> {
    with_router(|r| {
        if r.shortcut.is_some_and(|(id, _)| id == owner) {
            r.shortcut.take().map(|(_, command)| command)
        } else {
            None
        }
    })
}

/// Called by the foreground window's UI thread after native message dispatch.
pub(super) fn finish_lock_releases(owner: u64) {
    with_router(|r| {
        if unsafe { GetForegroundWindow() } != HWND(owner as _) {
            return;
        }
        let pending = std::mem::take(&mut r.lock_releases);
        for (id, key, generation) in pending {
            if id != owner {
                r.lock_releases.push((id, key, generation));
                continue;
            }
            if let Some(target) = &mut r.target
                && target.owner == id
                && target.input.keyboard_generation() == generation
            {
                let lock = ((unsafe { GetKeyState(i32::from(key)) } & 1) << 7) as u8;
                target.input.key(owner, key, false, Some(lock));
                target.generation = target.input.keyboard_generation();
            }
        }
    });
}
