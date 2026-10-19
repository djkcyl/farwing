use super::{
    locks,
    pointers::Pointers,
    wire::{Command, Event, PenPhase, PenSample, TouchContact, TouchPhase},
};
use crate::platform::windows::input::{hid, keyboard, system};
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    time::{Duration, Instant},
};
use windows::Win32::{
    Foundation::RECT,
    UI::{Input::KeyboardAndMouse::*, WindowsAndMessaging::*},
};

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Screen {
    pub id: i32,
    pub left: i32,
    pub top: i32,
    pub width: u32,
    pub height: u32,
}
impl Screen {
    pub fn rect(&self) -> Result<RECT> {
        ensure!(
            self.width > 0 && self.height > 0 && self.width <= 65536 && self.height <= 65536,
            "屏幕尺寸无效"
        );
        Ok(RECT {
            left: self.left,
            top: self.top,
            right: self
                .left
                .checked_add(self.width as i32)
                .ok_or_else(|| anyhow::anyhow!("屏幕坐标溢出"))?,
            bottom: self
                .top
                .checked_add(self.height as i32)
                .ok_or_else(|| anyhow::anyhow!("屏幕坐标溢出"))?,
        })
    }
}
#[derive(Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Geometry {
    pub current: i32,
    pub screens: Vec<Screen>,
}
impl Geometry {
    pub fn validate(&self) -> Result<()> {
        ensure!(self.screens.len() <= 32, "输入屏幕数量无效");
        let mut ids = std::collections::BTreeSet::new();
        for screen in &self.screens {
            screen.rect()?;
            ensure!(ids.insert(screen.id), "重复输入屏幕");
        }
        Ok(())
    }
    pub fn screen(&self, id: Option<i32>) -> Result<&Screen> {
        self.screens
            .iter()
            .find(|s| s.id == id.unwrap_or(self.current))
            .ok_or_else(|| anyhow::anyhow!("输入目标屏幕已失效"))
    }
}
#[derive(Clone, Copy)]
enum KeyRoute {
    /// SendInput by scan code.
    System,
    Hid(u8),
    Consumer(u8),
}
#[derive(Clone, Copy, PartialEq)]
enum MouseRoute {
    System,
    Hid,
}
#[derive(Clone, Copy)]
struct HeldKey {
    route: KeyRoute,
    asserted: bool,
}
#[derive(Clone, Copy)]
enum MouseMode {
    Relative,
    Absolute,
}

/// Keyboard Backspace; a stalled stream releases it early.
const BACKSPACE: u32 = keyboard::KEYBOARD | 0x2a;
/// Lower bound of the held-key timeout (§7).
pub(crate) const MIN_HOLD: Duration = Duration::from_millis(150);

pub(crate) struct Engine {
    device: Option<hid::Device>,
    /// Held keys by HID usage.
    keys: BTreeMap<u32, HeldKey>,
    buttons: u8,
    mouse_route: Option<MouseRoute>,
    mouse_mode: MouseMode,
    geometry: Geometry,
    desktop: system::Desktop,
    last_heartbeat: Instant,
    last_input: Instant,
    last_key: Instant,
    interruptible: bool,
    interrupted: bool,
    backspace: bool,
    /// Held keys are released after this long without any INPUT frame.
    threshold: Duration,
    lock_target: Option<locks::Target>,
    last_lock: Option<Instant>,
    sas: bool,
    hardware: bool,
    probe_at: Instant,
    probes: u8,
    origin: u32,
    hid_failed: bool,
    configuration: super::config::Configuration,
    foreground: Option<u32>,
    mouse_policy: super::config::MousePolicy,
    game_simulation: Option<bool>,
    route_diagnostics: u16,
    privileged: bool,
    /// Touch contacts and the pen (synthetic pointers).
    pointers: Pointers,
    /// The contact driving the left button when touch falls back to the
    /// mouse (no synthetic pointers, or aimed at the originating viewer).
    touch_mouse: Option<u8>,
    /// The button the pen holds when it falls back to the mouse.
    pen_mouse: Option<u8>,
}
impl Engine {
    pub fn new(hardware: bool, origin: u32, privileged: bool) -> Result<Self> {
        let now = Instant::now();
        Ok(Self {
            device: None,
            keys: BTreeMap::new(),
            buttons: 0,
            mouse_route: None,
            mouse_mode: MouseMode::Absolute,
            geometry: Default::default(),
            desktop: system::Desktop::new()?,
            last_heartbeat: now,
            last_input: now,
            last_key: now,
            interruptible: false,
            interrupted: false,
            backspace: false,
            threshold: MIN_HOLD,
            lock_target: None,
            last_lock: None,
            sas: false,
            hardware,
            probe_at: now,
            probes: 0,
            origin,
            hid_failed: false,
            configuration: Default::default(),
            foreground: None,
            mouse_policy: Default::default(),
            game_simulation: None,
            route_diagnostics: 0,
            privileged,
            pointers: Pointers::new(),
            touch_mouse: None,
            pen_mouse: None,
        })
    }
    pub fn backend(&self) -> &'static str {
        if self.device.is_some() {
            "Farwing HID"
        } else if self.privileged {
            "Windows · 系统服务"
        } else {
            "Windows · 当前用户"
        }
    }
    pub fn take_sas(&mut self) -> bool {
        std::mem::take(&mut self.sas)
    }
    /// The current owner's held-key timeout, derived from its RTT.
    pub fn hold_timeout(&mut self, timeout: Duration) {
        self.threshold = timeout.clamp(MIN_HOLD, Duration::from_secs(10));
    }
    pub fn configure(&mut self, configuration: super::config::Configuration) -> Result<()> {
        configuration.validate()?;
        self.configuration = configuration;
        self.foreground = None;
        if !self.configuration.keyboard_interrupts {
            self.interruptible = false;
        }
        self.observe_foreground();
        Ok(())
    }
    pub fn mouse_policy(&self) -> super::config::MousePolicy {
        self.mouse_policy.clone()
    }
    fn observe_foreground(&mut self) {
        let mut pid = 0;
        unsafe {
            GetWindowThreadProcessId(GetForegroundWindow(), Some(&mut pid));
        }
        if self.foreground == Some(pid) {
            return;
        }
        self.foreground = Some(pid);
        let name = crate::platform::windows::host_service::process::image(pid)
            .ok()
            .and_then(|path| {
                path.file_name()
                    .map(|name| name.to_string_lossy().to_lowercase())
            });
        let rule = name
            .as_ref()
            .and_then(|name| self.configuration.games.get(name));
        let mode = rule.map_or(2, |rule| rule.mode);
        self.game_simulation = rule.map(|rule| rule.simulation);
        if self.mouse_policy.mode.unwrap_or(2) == mode {
            return;
        }
        let mut restore = None;
        if mode == 2
            && let Ok(screen) = self.geometry.screen(None)
        {
            let mut point = windows::Win32::Foundation::POINT::default();
            if unsafe { GetCursorPos(&mut point) }.is_ok()
                && let Ok(rect) = screen.rect()
                && point.x >= rect.left
                && point.x < rect.right
                && point.y >= rect.top
                && point.y < rect.bottom
            {
                restore = Some([
                    (point.x - rect.left) as f64 / screen.width as f64,
                    (point.y - rect.top) as f64 / screen.height as f64,
                ]);
            }
        }
        self.mouse_policy = super::config::MousePolicy {
            mode: Some(mode),
            restore,
        };
    }
    fn hid_allowed(&self) -> bool {
        // Input aimed back at its originating UI uses marked system events, so
        // it cannot be captured and retransmitted by the same viewer indefinitely.
        let mut foreground = 0;
        unsafe {
            GetWindowThreadProcessId(GetForegroundWindow(), Some(&mut foreground));
        }
        self.device.is_some() && foreground != self.origin
    }
    pub fn synchronize(&mut self, geometry: Geometry) -> Result<()> {
        if geometry != self.geometry {
            self.release()?;
            self.geometry = geometry;
        }
        self.observe_foreground();
        if let Some((desktop, name)) = self.desktop.changed()? {
            let release = self.release();
            // Synthetic devices are rebuilt on the new desktop when used.
            self.pointers.close_devices();
            self.desktop.switch(desktop, name)?;
            if release.is_err() {
                self.release()?;
            }
            // HID stays off on secure desktops. The service's native path owns
            // those desktops; switching occurs only after old holds are released.
            self.device.take();
            self.probes = 0;
            self.probe_at = Instant::now();
        }
        if self.hardware
            && self.desktop.ordinary()
            && self.device.is_none()
            && self.probes < 5
            && Instant::now() >= self.probe_at
            && self.keys.is_empty()
            && self.buttons == 0
        {
            self.probes += 1;
            self.probe_at = Instant::now() + Duration::from_secs(1u64 << (self.probes - 1));
            match hid::Device::open() {
                Ok(device) => self.device = device,
                Err(error) => {
                    tracing::warn!(%error,"host HID preparation failed; system input retained")
                }
            }
        }
        // Every input request passes here, including absolute/system mouse
        // events and remote heartbeats. A busy broker may never become idle or
        // receive Request::Alive; HID ownership must not depend on either.
        if let Some(device) = &mut self.device
            && let Err(error) = device.tick()
        {
            tracing::warn!(%error,"host HID lease ended; releasing old input state");
            self.recover_hid()?;
        }
        Ok(())
    }
    fn keyboard_report(&mut self, consumer: bool) -> Result<()> {
        if consumer {
            let mut report = [6, 0];
            for key in self.keys.values().filter(|k| k.asserted) {
                if let KeyRoute::Consumer(bit) = key.route {
                    report[1] |= 1 << bit;
                }
            }
            self.hid_report(&report)?;
            return Ok(());
        }
        let mut report = [0u8; 33];
        report[0] = 1;
        for key in self.keys.values().filter(|k| k.asserted) {
            if let KeyRoute::Hid(usage) = key.route {
                report[1 + usize::from(usage) / 8] |= 1 << (usage % 8);
            }
        }
        self.hid_report(&report)?;
        Ok(())
    }
    fn key_route(&self, usage: u32) -> Result<KeyRoute> {
        if self.hid_allowed() {
            if let Some(bit) = hid::consumer(usage) {
                return Ok(KeyRoute::Consumer(bit));
            }
            if let Some(id) = hid::keyboard(usage) {
                return Ok(KeyRoute::Hid(id));
            }
        }
        ensure!(keyboard::scan(usage).is_some(), "无效键码");
        Ok(KeyRoute::System)
    }
    fn system_key(usage: u32, down: bool) -> Result<()> {
        let scan = keyboard::scan(usage).ok_or_else(|| anyhow::anyhow!("无效键码"))?;
        system::key(scan, down)
    }
    fn key(&mut self, usage: u32, down: bool) -> Result<()> {
        if keyboard::lock_index(usage).is_some() {
            self.last_lock = Some(Instant::now());
        }
        if down {
            let route = match self.keys.get(&usage) {
                Some(held) => held.route,
                None => self.key_route(usage)?,
            };
            if matches!(route, KeyRoute::System) {
                Self::system_key(usage, true)?;
                self.keys.insert(
                    usage,
                    HeldKey {
                        route,
                        asserted: true,
                    },
                );
            } else {
                let old = self.keys.insert(
                    usage,
                    HeldKey {
                        route,
                        asserted: true,
                    },
                );
                if let Err(error) = self.keyboard_report(matches!(route, KeyRoute::Consumer(_))) {
                    if let Some(old) = old {
                        self.keys.insert(usage, old);
                    }
                    // A timed-out driver request may already have delivered its
                    // report. Keep the new down's release obligation for recovery.
                    return Err(error);
                }
            }
        } else if let Some(held) = self.keys.get(&usage).copied() {
            if !held.asserted {
                self.keys.remove(&usage);
                return Ok(());
            }
            if matches!(held.route, KeyRoute::System) {
                Self::system_key(usage, false)?;
                self.keys.remove(&usage);
            } else {
                self.keys.remove(&usage);
                if let Err(error) =
                    self.keyboard_report(matches!(held.route, KeyRoute::Consumer(_)))
                {
                    self.keys.insert(usage, held);
                    return Err(error);
                }
            }
        }
        Ok(())
    }
    /// Apply a pending `LockState` once this participant's own lock presses
    /// have settled; one attempt per `LockState`.
    fn reconcile_locks(&mut self, permitted: &impl Fn() -> bool) -> Result<()> {
        let Some(target) = self.lock_target else {
            return Ok(());
        };
        let held = keyboard::LOCKS
            .iter()
            .any(|lock| self.keys.contains_key(lock));
        match target.due(Instant::now(), self.last_lock, held) {
            locks::Due::Wait => return Ok(()),
            locks::Due::Expired => {
                self.lock_target = None;
                return Ok(());
            }
            locks::Due::Now => self.lock_target = None,
        }
        for usage in target.corrections(system::locks()) {
            ensure!(permitted(), "被控输入许可已撤销");
            self.key(usage, true)?;
            self.key(usage, false)?;
        }
        Ok(())
    }
    /// Host shortcuts by virtual key, independent of the host's layout.
    fn shortcut(keys: &[u16], permitted: &impl Fn() -> bool) -> Result<()> {
        let mut pressed = 0;
        let result = keys.iter().try_for_each(|&key| {
            ensure!(permitted(), "快捷输入已撤销");
            system::virtual_key(key, true)?;
            pressed += 1;
            Ok(())
        });
        // Release whatever was pressed, in reverse order, even after a failure.
        for &key in keys[..pressed].iter().rev() {
            system::virtual_key(key, false)?;
        }
        result
    }
    fn mouse_report(&mut self, x: i16, y: i16, wheel: i16, horizontal: i16) -> Result<()> {
        let mut r = vec![5, self.buttons];
        for n in [x, y, wheel, horizontal] {
            r.extend(n.to_le_bytes());
        }
        self.hid_report(&r)
    }
    fn hid_report(&mut self, report: &[u8]) -> Result<()> {
        let result = self
            .device
            .as_mut()
            .ok_or_else(|| anyhow::anyhow!("HID设备已失效"))?
            .report(report);
        self.hid_failed |= result.is_err();
        result
    }
    fn recover_hid(&mut self) -> Result<()> {
        // Closing the old file triggers driver neutralization. Also discharge
        // owned Win32 key/button state; never repeat a down or a pointer movement.
        self.device.take();
        for held in self.keys.values_mut() {
            if !matches!(held.route, KeyRoute::System) {
                held.route = KeyRoute::System;
            }
        }
        if self.mouse_route == Some(MouseRoute::Hid) {
            self.mouse_route = Some(MouseRoute::System);
        }
        self.hid_failed = false;
        self.probes = 0;
        self.probe_at = Instant::now() + Duration::from_secs(1);
        self.release()
    }
    fn button(&mut self, button: u8, down: bool) -> Result<()> {
        let bit = match button {
            1 => 1,
            2 => 2,
            16 => 4,
            32 => 8,
            64 => 16,
            _ => anyhow::bail!("无效鼠标按键"),
        };
        if !down && self.buttons & bit == 0 {
            return Ok(());
        }
        let route = self.mouse_route();
        if down {
            // Record each route/coordinate/keyboard combination only once per
            // session. No key codes, text or foreground application names.
            let signature = u32::from(route == MouseRoute::Hid)
                | (u32::from(matches!(self.mouse_mode, MouseMode::Relative)) << 1)
                | (u32::from(!self.keys.is_empty()) << 2)
                | (u32::from(button == 1) << 3);
            let bit = 1u16 << signature;
            if self.route_diagnostics & bit == 0 {
                self.route_diagnostics |= bit;
                tracing::debug!(
                    hid = route == MouseRoute::Hid,
                    relative = matches!(self.mouse_mode, MouseMode::Relative),
                    keyboard_held = !self.keys.is_empty(),
                    primary = button == 1,
                    legacy_simulation = ?self.game_simulation,
                    "host mouse button injection route"
                );
            }
        }
        let previous = self.buttons;
        if down {
            self.buttons |= bit
        } else {
            self.buttons &= !bit
        }
        let result = match route {
            MouseRoute::System => system::button(button, down),
            MouseRoute::Hid => self.mouse_report(0, 0, 0, 0),
        };
        if let Err(e) = result {
            self.buttons = previous;
            return Err(e);
        }
        self.mouse_route = if self.buttons == 0 { None } else { Some(route) };
        Ok(())
    }
    fn mouse_route(&self) -> MouseRoute {
        self.mouse_route.unwrap_or(
            // Coordinates and injection backend are independent. Prefer the
            // same owned HID path as physical keys for mouse buttons, including
            // absolute-position control. The wire simulation hint selects an
            // injector implementation; it is not a local HID permission.
            // Preserve system fallback and origin-loop prevention, and retain
            // the down's route until every mouse button has been released.
            if self.hid_allowed() {
                MouseRoute::Hid
            } else {
                MouseRoute::System
            },
        )
    }
    fn relative(&mut self, mut x: i32, mut y: i32, permitted: &impl Fn() -> bool) -> Result<()> {
        ensure!(
            x.unsigned_abs() <= 1_000_000 && y.unsigned_abs() <= 1_000_000,
            "相对位移过大"
        );
        if self.buttons == 0 {
            self.mouse_mode = MouseMode::Relative;
        }
        // A drag stays on the collection that accepted its initial button down.
        if self.mouse_route() == MouseRoute::System
            || matches!(self.mouse_mode, MouseMode::Absolute)
        {
            return system::mouse(x, y, 0, MOUSEEVENTF_MOVE);
        }
        while x != 0 || y != 0 {
            ensure!(permitted(), "输入已取消");
            let dx = x.clamp(-32767, 32767) as i16;
            let dy = y.clamp(-32767, 32767) as i16;
            self.mouse_report(dx, dy, 0, 0)?;
            x -= i32::from(dx);
            y -= i32::from(dy);
        }
        Ok(())
    }
    fn absolute(&mut self, screen: Option<i32>, x: f64, y: f64) -> Result<()> {
        let r = self.geometry.screen(screen)?.rect()?;
        let (left, top, w, h) = unsafe {
            (
                GetSystemMetrics(SM_XVIRTUALSCREEN),
                GetSystemMetrics(SM_YVIRTUALSCREEN),
                GetSystemMetrics(SM_CXVIRTUALSCREEN),
                GetSystemMetrics(SM_CYVIRTUALSCREEN),
            )
        };
        ensure!(w > 1 && h > 1, "桌面范围无效");
        let (px, py) = super::pointers::pixel(r, x, y);
        system::mouse(
            ((f64::from(px) - left as f64) * 65535.0 / (w - 1) as f64).round() as i32,
            ((f64::from(py) - top as f64) * 65535.0 / (h - 1) as f64).round() as i32,
            0,
            MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK,
        )?;
        if self.buttons == 0 {
            self.mouse_mode = MouseMode::Absolute;
        }
        Ok(())
    }
    /// The window under a desktop point belongs to the process that started
    /// this control (a viewer of this host): synthetic pointers there would
    /// be captured and sent again.
    fn origin_at(&self, rect: RECT, x: f64, y: f64) -> bool {
        let (px, py) = super::pointers::pixel(rect, x, y);
        let mut pid = 0;
        unsafe {
            let window = WindowFromPoint(windows::Win32::Foundation::POINT { x: px, y: py });
            GetWindowThreadProcessId(window, Some(&mut pid));
        }
        pid != 0 && pid == self.origin
    }
    fn touch(&mut self, screen: i32, contacts: &[TouchContact]) -> Result<()> {
        let rect = self.geometry.screen(Some(screen))?.rect()?;
        let synthetic = self.touch_mouse.is_none()
            && Pointers::available()
            && (self.pointers.active()
                || contacts
                    .iter()
                    .find(|c| c.phase == TouchPhase::Down)
                    .is_none_or(|c| !self.origin_at(rect, c.x, c.y)));
        tracing::debug!(synthetic, contacts = contacts.len(), "host touch frame");
        if synthetic {
            return self.pointers.touch(rect, contacts);
        }
        // Mouse fallback: the first contact drives the left button.
        if contacts.is_empty() {
            if self.touch_mouse.take().is_some() {
                self.button(1, false)?;
            }
            return Ok(());
        }
        match self.touch_mouse {
            None => {
                if let Some(c) = contacts.iter().find(|c| c.phase == TouchPhase::Down) {
                    self.absolute(Some(screen), c.x, c.y)?;
                    self.button(1, true)?;
                    self.touch_mouse = Some(c.id);
                }
            }
            Some(id) => match contacts.iter().find(|c| c.id == id) {
                None => {
                    self.touch_mouse = None;
                    self.button(1, false)?;
                }
                Some(c) => {
                    self.absolute(Some(screen), c.x, c.y)?;
                    if matches!(c.phase, TouchPhase::Up | TouchPhase::Cancel) {
                        self.touch_mouse = None;
                        self.button(1, false)?;
                    }
                }
            },
        }
        Ok(())
    }
    fn pen(&mut self, screen: i32, samples: &[PenSample]) -> Result<()> {
        let rect = self.geometry.screen(Some(screen))?.rect()?;
        let synthetic = self.pen_mouse.is_none()
            && Pointers::available()
            && (self.pointers.active()
                || samples
                    .first()
                    .is_none_or(|s| !self.origin_at(rect, s.x, s.y)));
        tracing::debug!(synthetic, samples = samples.len(), "host pen frame");
        if synthetic {
            return self.pointers.pen(rect, samples);
        }
        // Mouse fallback: tip = left button, barrel = right, hover = move.
        for s in samples {
            match s.phase {
                PenPhase::Hover | PenPhase::Move => self.absolute(Some(screen), s.x, s.y)?,
                PenPhase::Down => {
                    self.absolute(Some(screen), s.x, s.y)?;
                    if self.pen_mouse.is_none() {
                        let button = if s.barrel { 2 } else { 1 };
                        self.button(button, true)?;
                        self.pen_mouse = Some(button);
                    }
                }
                PenPhase::Up | PenPhase::Leave | PenPhase::Cancel => {
                    if s.phase == PenPhase::Up {
                        self.absolute(Some(screen), s.x, s.y)?;
                    }
                    if let Some(button) = self.pen_mouse.take() {
                        self.button(button, false)?;
                    }
                }
            }
        }
        Ok(())
    }
    pub fn apply(
        &mut self,
        event: Event,
        geometry: Geometry,
        permitted: impl Fn() -> bool,
    ) -> Result<()> {
        let result = self.apply_inner(event, geometry, &permitted);
        if self.hid_failed {
            self.recover_hid()?;
        }
        result
    }
    fn apply_inner(
        &mut self,
        event: Event,
        geometry: Geometry,
        permitted: &impl Fn() -> bool,
    ) -> Result<()> {
        ensure!(permitted(), "被控输入许可已撤销");
        self.synchronize(geometry)?;
        ensure!(permitted(), "被控输入许可已撤销");
        if matches!(event, Event::Heartbeat) {
            self.last_heartbeat = Instant::now();
            return Ok(());
        }
        self.last_input = Instant::now();
        match event {
            Event::Command(command) => {
                self.release()?;
                ensure!(permitted(), "被控输入许可已撤销");
                match command {
                    // The service performs the sequence after this request.
                    Command::SecureAttention => self.sas = true,
                    Command::Lock => unsafe {
                        windows::Win32::System::Shutdown::LockWorkStation()?;
                    },
                    Command::Desktop => Self::shortcut(&[0x5b, 0x44], permitted)?,
                    Command::TaskView => Self::shortcut(&[0x5b, 0x09], permitted)?,
                    Command::TaskManager => Self::shortcut(&[0x11, 0x10, 0x1b], permitted)?,
                }
            }
            Event::Heartbeat => {}
            Event::Key {
                usage,
                down,
                repeat,
            } => {
                self.last_key = self.last_input;
                self.interruptible = self.configuration.keyboard_interrupts;
                self.interrupted = false;
                self.backspace = usage == BACKSPACE && down;
                if down
                    && !repeat
                    && let Some(target) = &mut self.lock_target
                {
                    target.pressed(usage);
                }
                // The host's own HID stack repeats a held HID key; SendInput
                // keys repeat by injecting another down.
                let hid = self
                    .keys
                    .get(&usage)
                    .is_some_and(|held| held.asserted && !matches!(held.route, KeyRoute::System));
                if !(repeat && hid) {
                    self.key(usage, down)?;
                }
            }
            Event::LockState {
                caps, num, scroll, ..
            } => {
                self.lock_target = Some(locks::Target::new([caps, num, scroll], Instant::now()));
                self.reconcile_locks(permitted)?;
            }
            Event::Button { button, down } => self.button(button, down)?,
            Event::Relative { x, y } => self.relative(x, y, &permitted)?,
            Event::Absolute { screen, x, y } => self.absolute(screen, x, y)?,
            Event::Touch { screen, contacts } => self.touch(screen, &contacts)?,
            Event::Pen { screen, samples } => self.pen(screen, &samples)?,
            Event::PointerCancel => self.cancel_pointers()?,
            Event::Wheel { x, y } => {
                // HID wheel units are detents. Preserve fine wheel input through
                // the system path; never round a touchpad increment into a notch.
                let detents = |delta: i32| {
                    (delta % 120 == 0)
                        .then(|| i16::try_from(delta / 120).ok())
                        .flatten()
                };
                if self.mouse_route() == MouseRoute::Hid
                    && matches!(self.mouse_mode, MouseMode::Relative)
                    && let (Some(vertical), Some(horizontal)) = (detents(y), detents(x))
                {
                    if x != 0 || y != 0 {
                        self.mouse_report(0, 0, vertical, horizontal)?;
                    }
                } else {
                    if y != 0 {
                        system::mouse(0, 0, y as u32, MOUSEEVENTF_WHEEL)?;
                    }
                    if x != 0 {
                        system::mouse(0, 0, x as u32, MOUSEEVENTF_HWHEEL)?;
                    }
                }
            }
        }
        Ok(())
    }
    /// Lift every contact and the pen, synthetic or mouse fallback.
    fn cancel_pointers(&mut self) -> Result<()> {
        let synthetic = self.pointers.cancel();
        if self.touch_mouse.take().is_some() {
            self.button(1, false)?;
        }
        if let Some(button) = self.pen_mouse.take() {
            self.button(button, false)?;
        }
        synthetic
    }
    pub fn tick(&mut self) -> Result<()> {
        self.synchronize(self.geometry.clone())?;
        // Contacts time out regardless of the keyboard-interrupt setting.
        let quiet = self.last_heartbeat.max(self.last_input).elapsed();
        self.pointers.tick(quiet, self.threshold)?;
        if self.interruptible && !self.interrupted && !self.keys.is_empty() {
            // Heartbeats and every other INPUT frame keep held keys alive.
            let quiet = self.last_heartbeat.max(self.last_input).elapsed();
            if quiet > self.threshold
                || (self.backspace
                    && quiet > Duration::from_millis(100)
                    && self.last_key.elapsed() > Duration::from_millis(100))
            {
                self.neutral_keys(self.backspace)?;
                self.interrupted = true;
            }
        }
        self.reconcile_locks(&|| true)
    }
    pub fn release(&mut self) -> Result<()> {
        if self.hid_failed {
            return self.recover_hid();
        }
        let mut failure = None;
        if let Err(e) = self.pointers.cancel() {
            failure = Some(e);
        }
        self.touch_mouse = None;
        self.pen_mouse = None;
        if let Err(e) = self.neutral_keys(true) {
            failure = Some(e);
        }
        for (bit, button) in [(1, 1), (2, 2), (4, 16), (8, 32), (16, 64)] {
            if self.buttons & bit != 0 {
                if let Err(e) = self.button(button, false) {
                    failure = Some(e);
                }
            }
        }
        if self.buttons == 0 {
            self.mouse_route = None;
        }
        // A pending lock correction belongs to the owner being released.
        self.lock_target = None;
        self.interruptible = false;
        self.backspace = false;
        if let Some(e) = failure {
            Err(e)
        } else {
            Ok(())
        }
    }
    fn neutral_keys(&mut self, clear: bool) -> Result<()> {
        let mut failure = None;
        for (key, held) in self.keys.clone() {
            match self.key(key, false) {
                Ok(()) if !clear => {
                    self.keys.insert(
                        key,
                        HeldKey {
                            asserted: false,
                            ..held
                        },
                    );
                }
                Ok(()) => {}
                Err(e) => failure = Some(e),
            }
        }
        if let Some(e) = failure {
            Err(e)
        } else {
            Ok(())
        }
    }
}
impl Drop for Engine {
    fn drop(&mut self) {
        let _ = self.release();
    }
}
