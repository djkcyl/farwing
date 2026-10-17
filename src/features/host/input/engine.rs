use super::wire::{Event, KeyOrButton};
use crate::platform::windows::input::{hid, system};
use anyhow::{Result, ensure};
use std::{
    collections::BTreeMap,
    time::{Duration, Instant},
};
use windows::Win32::{
    Foundation::RECT,
    UI::{Input::KeyboardAndMouse::*, WindowsAndMessaging::*},
};

pub(crate) use super::geometry::Geometry;
fn native_rect(r: super::geometry::Rect) -> RECT {
    RECT {
        left: r.left,
        top: r.top,
        right: r.right,
        bottom: r.bottom,
    }
}

#[derive(Clone, Copy)]
enum KeyRoute {
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

pub(crate) struct Engine {
    device: Option<hid::Device>,
    keys: BTreeMap<u16, HeldKey>,
    buttons: u8,
    mouse_route: Option<MouseRoute>,
    mouse_mode: MouseMode,
    touch: system::Touch,
    geometry: Geometry,
    desktop: system::Desktop,
    last_heartbeat: Instant,
    last_input: Instant,
    last_key: Instant,
    interruptible: bool,
    interrupted: bool,
    backspace: bool,
    threshold: Duration,
    sas: bool,
    hardware: bool,
    probe_at: Instant,
    probes: u8,
    mobile: bool,
    origin: u32,
    hid_failed: bool,
    configuration: super::config::Configuration,
    foreground: Option<u32>,
    mouse_policy: super::config::MousePolicy,
    game_simulation: Option<bool>,
    route_diagnostics: u16,
    privileged: bool,
}
impl Engine {
    pub fn new(
        hardware: bool,
        policy: super::wire::Policy,
        origin: u32,
        privileged: bool,
    ) -> Result<Self> {
        let now = Instant::now();
        Ok(Self {
            device: None,
            keys: BTreeMap::new(),
            buttons: 0,
            mouse_route: None,
            mouse_mode: MouseMode::Absolute,
            touch: Default::default(),
            geometry: Default::default(),
            desktop: system::Desktop::new()?,
            last_heartbeat: now,
            last_input: now,
            last_key: now,
            interruptible: false,
            interrupted: false,
            backspace: false,
            threshold: Duration::from_millis(if matches!(policy, super::wire::Policy::Windows) {
                150
            } else {
                300
            }),
            sas: false,
            hardware,
            probe_at: now,
            probes: 0,
            mobile: matches!(policy, super::wire::Policy::Mobile),
            origin,
            hid_failed: false,
            configuration: Default::default(),
            foreground: None,
            mouse_policy: Default::default(),
            game_simulation: None,
            route_diagnostics: 0,
            privileged,
        })
    }
    pub fn backend(&self) -> &'static str {
        if self.device.is_some() {
            "OpenUUYC HID"
        } else if self.privileged {
            "Windows · 系统服务"
        } else {
            "Windows · 当前用户"
        }
    }
    pub fn take_sas(&mut self) -> bool {
        std::mem::take(&mut self.sas)
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
    fn key(&mut self, key: u16, down: bool) -> Result<()> {
        if down {
            let route = self.keys.get(&key).map(|k| k.route).unwrap_or_else(|| {
                if self.hid_allowed() {
                    if let Some(bit) = hid::consumer(key) {
                        return KeyRoute::Consumer(bit);
                    }
                    if let Some(usage) = hid::usage(key) {
                        return KeyRoute::Hid(usage);
                    }
                }
                KeyRoute::System
            });
            if matches!(route, KeyRoute::System) {
                system::keyboard(key, true)?;
                self.keys.insert(
                    key,
                    HeldKey {
                        route,
                        asserted: true,
                    },
                );
            } else {
                let old = self.keys.insert(
                    key,
                    HeldKey {
                        route,
                        asserted: true,
                    },
                );
                if let Err(error) = self.keyboard_report(matches!(route, KeyRoute::Consumer(_))) {
                    if let Some(old) = old {
                        self.keys.insert(key, old);
                    }
                    // A timed-out driver request may already have delivered its
                    // report. Keep the new down's release obligation for recovery.
                    return Err(error);
                }
            }
        } else if let Some(held) = self.keys.get(&key).copied() {
            if !held.asserted {
                self.keys.remove(&key);
                return Ok(());
            }
            if matches!(held.route, KeyRoute::System) {
                system::keyboard(key, false)?;
                self.keys.remove(&key);
            } else {
                self.keys.remove(&key);
                if let Err(error) =
                    self.keyboard_report(matches!(held.route, KeyRoute::Consumer(_)))
                {
                    self.keys.insert(key, held);
                    return Err(error);
                }
            }
        }
        Ok(())
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
            // absolute-position control. The official simulation hint selects
            // its injector implementation; it is not a local HID permission.
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
                use super::wire::Command;
                self.release()?;
                ensure!(permitted(), "被控输入许可已撤销");
                let keys: &[u16] = match command {
                    Command::Desktop => &[0x5b, 0x44],
                    Command::TaskView => &[0x5b, 0x09],
                    Command::TaskManager => &[0x11, 0x10, 0x1b],
                    Command::Lock => {
                        unsafe {
                            windows::Win32::System::Shutdown::LockWorkStation()?;
                        }
                        return Ok(());
                    }
                };
                for &key in keys {
                    ensure!(permitted(), "快捷输入已撤销");
                    self.key(key, true)?;
                }
                for &key in keys.iter().rev() {
                    self.key(key, false)?;
                }
            }
            Event::Heartbeat => {}
            Event::Key {
                key,
                down,
                interruptible,
                toggle,
            } => {
                if down && key == 0x4c && [0x5b, 0x5c].iter().any(|k| self.keys.contains_key(k)) {
                    self.release()?;
                    ensure!(permitted(), "被控输入许可已撤销");
                    unsafe {
                        windows::Win32::System::Shutdown::LockWorkStation()?;
                    }
                    return Ok(());
                }
                if down
                    && key == 0x2e
                    && [0x11, 0xa2, 0xa3].iter().any(|k| self.keys.contains_key(k))
                    && [0x12, 0xa4, 0xa5].iter().any(|k| self.keys.contains_key(k))
                {
                    self.release()?;
                    ensure!(permitted(), "被控输入许可已撤销");
                    self.sas = true;
                    return Ok(());
                }
                self.last_key = self.last_input;
                self.interruptible = interruptible && self.configuration.keyboard_interrupts;
                self.interrupted = false;
                self.backspace = key == 8 && down;
                if let Some(wanted) = toggle {
                    if !down && system::toggle(key) != wanted {
                        self.key(key, true)?;
                        self.key(key, false)?;
                    }
                } else {
                    self.key(key, down)?;
                }
            }
            Event::Button { button, down } => self.button(button, down)?,
            Event::Click { button } => {
                self.button(button, true)?;
                self.button(button, false)?;
            }
            Event::Relative { x, y } => self.relative(x, y, &permitted)?,
            Event::RelativeScaled { screen, x, y } => {
                let s = self.geometry.screen(screen)?;
                let (w, h) = unsafe {
                    (
                        GetSystemMetrics(SM_CXVIRTUALSCREEN),
                        GetSystemMetrics(SM_CYVIRTUALSCREEN),
                    )
                };
                ensure!(w > 0 && h > 0, "桌面范围无效");
                self.relative(
                    (s.width as f64 / w as f64 * 65535.0 * x) as i32,
                    (s.height as f64 / h as f64 * 65535.0 * y) as i32,
                    &permitted,
                )?;
            }
            Event::Absolute { screen, x, y } => {
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
                let px = (r.left as f64 + (r.right - r.left) as f64 * x)
                    .round()
                    .clamp(r.left as f64, (r.right - 1) as f64);
                let py = (r.top as f64 + (r.bottom - r.top) as f64 * y)
                    .round()
                    .clamp(r.top as f64, (r.bottom - 1) as f64);
                system::mouse(
                    ((px - left as f64) * 65535.0 / (w - 1) as f64).round() as i32,
                    ((py - top as f64) * 65535.0 / (h - 1) as f64).round() as i32,
                    0,
                    MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK,
                )?;
                if self.buttons == 0 {
                    self.mouse_mode = MouseMode::Absolute;
                }
            }
            Event::Wheel { x, y } => {
                let (horizontal, delta) = if x.unsigned_abs() > y.unsigned_abs() {
                    (true, x)
                } else {
                    (false, y)
                };
                if delta != 0 {
                    // HID wheel units are detents. Preserve fine wheel input through
                    // the system path; never round a touchpad increment into a notch.
                    if self.mouse_route() == MouseRoute::Hid
                        && matches!(self.mouse_mode, MouseMode::Relative)
                        && delta % 120 == 0
                        && i16::try_from(delta / 120).is_ok()
                    {
                        let n = (delta / 120) as i16;
                        self.mouse_report(
                            0,
                            0,
                            if horizontal { 0 } else { n },
                            if horizontal { n } else { 0 },
                        )?;
                    } else {
                        system::mouse(
                            0,
                            0,
                            delta as u32,
                            if horizontal {
                                MOUSEEVENTF_HWHEEL
                            } else {
                                MOUSEEVENTF_WHEEL
                            },
                        )?;
                    }
                }
            }
            Event::Text(text) => system::text(&text, &permitted)?,
            Event::Combination { down, keys } => {
                for item in keys {
                    ensure!(permitted(), "组合输入已取消");
                    match item {
                        KeyOrButton::Key(k) => self.key(k, down)?,
                        KeyOrButton::Button(b) => self.button(b, down)?,
                    }
                }
            }
            Event::Touch { kind, points } => {
                let r = self.geometry.screen(None)?.rect()?;
                self.touch.update(kind, &points, native_rect(r))?;
            }
        }
        Ok(())
    }
    pub fn tick(&mut self) -> Result<()> {
        self.synchronize(self.geometry.clone())?;
        if self.interruptible && !self.interrupted && !self.keys.is_empty() {
            let heartbeat = self.last_heartbeat.elapsed();
            let timeout = heartbeat > self.threshold && self.last_input <= self.last_heartbeat;
            if timeout
                || (self.backspace
                    && heartbeat > Duration::from_millis(100)
                    && self.last_key.elapsed() > Duration::from_millis(100))
            {
                self.neutral_keys(self.backspace || self.mobile)?;
                self.interrupted = true;
            }
        }
        self.touch.tick()
    }
    pub fn release(&mut self) -> Result<()> {
        if self.hid_failed {
            return self.recover_hid();
        }
        let mut failure = None;
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
        if let Err(e) = self.touch.cancel() {
            failure = Some(e);
        }
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
