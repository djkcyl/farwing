//! The Linux input engine: the same ownership contract as the Windows engine
//! (every down it sends is released on loss, interruption or disconnect) on
//! top of the X Test injector.
use super::geometry::{Geometry, Rect, Screen};
use super::wire::{Event, KeyOrButton};
use crate::platform::linux::input::{self as native, Injector, Lock};
use crate::platform::{CompositorLayout, Rotation};
use anyhow::{Result, ensure};
use std::{
    collections::BTreeSet,
    time::{Duration, Instant},
};

/// The UU wire button mask values, in the order X numbers them.
const BUTTONS: [(u8, u8, u8); 5] = [
    // (wire value, held bit, X button)
    (1, 1, 1),
    (2, 2, 3),
    (16, 4, 2),
    (32, 8, 8),
    (64, 16, 9),
];

pub(crate) struct Engine {
    injector: Injector,
    keys: BTreeSet<u16>,
    /// Keys released by an interruption, owed nothing further but still
    /// counted as the controller's until it lifts them.
    parked: BTreeSet<u16>,
    buttons: u8,
    touch: Option<u32>,
    wheel: [i32; 2],
    geometry: Geometry,
    last_heartbeat: Instant,
    last_input: Instant,
    last_key: Instant,
    interruptible: bool,
    interrupted: bool,
    backspace: bool,
    threshold: Duration,
    mobile: bool,
    configuration: super::config::Configuration,
    foreground: Option<Option<u32>>,
    mouse_policy: super::config::MousePolicy,
    /// Why absolute positions last went where they did, so a change is
    /// logged once rather than on every event.
    aim_space: Option<Result<(), String>>,
    /// The pointer was last placed through the absolute device, so the
    /// device's axis values are where the pointer is. After a relative move
    /// (or an absolute position that fell back to one) they are stale, and
    /// writing them again would throw the pointer back there.
    aimed: bool,
    /// 这一串触摸走哪条路（见 `touch`）。选好之后整串都走它，抬起后再清掉。
    touch_route: Option<Route>,
    /// 选路的结果，只为让日志在变化时记一条（和 `aim_space` 一样）。
    touch_space: Option<Result<(), String>>,
}

/// 一串触摸走哪条路。
#[derive(Clone, Copy, PartialEq, Debug)]
enum Route {
    /// 交给虚拟多点触摸屏。带上这一串要用的旋转与屏幕宽度，免得每一帧再查一次
    /// 屏幕——抬手指那一帧查不到屏幕也得照样发得出去。
    Touchscreen { rotation: Rotation, width: f64 },
    /// 退回单指指针模拟。模拟走绝对指针，落点总是对的。
    Simulated,
}

/// 这一串触摸该走哪条路。
///
/// 只有「合成器必然把这块触摸屏认到唯一那块屏上」时才敢用设备。合成器怎么给触摸屏
/// 和输出配对各家都不一样：Hyprland 找不到绑定就用当前聚焦的显示器（触点落在哪块屏
/// 取决于被控端此刻的焦点），mutter 按 EDID 与物理尺寸匹配、都匹配不上就归给内置屏，
/// KWin 又是另一套。猜错就是「主控看着这块屏、点下去落在另一块屏上」，比单指模拟更
/// 差——模拟走绝对指针，落点总是对的。所以三件事都得成立：
///
/// - 合成器布局拿得到（Wayland 会话），而且里面恰好一块输出；
/// - 被控端也恰好一块屏（`geometry.screens` 是被控端全部的屏幕，不只是主控正在
///   看的那块）；
/// - 合成器里这块输出没被转过：转过的屏上触摸屏坐标转不转，取决于合成器和用户的
///   配置。niri 和 KWin 总是按输出旋转；mutter 只在把触摸屏映射到某块屏时转，不
///   映射时铺满整个舞台、不转；wlroots（sway）只在配了 `map_to_output` 时转；
///   Hyprland 只在配了 `input:touchdevice:transform` 时转。这边看不出是哪一种，
///   反算错了就落错地方。
///
/// 以后某个合成器的映射能确切知道了（按合成器逐个放开、或者读 niri 的
/// `map-to-output`），再改这里。
fn route_for(layout: Option<&CompositorLayout>, screens: &[Screen]) -> Result<Route, String> {
    let Some(layout) = layout else {
        return Err("读不到合成器的输出布局".into());
    };
    let [output] = layout.outputs.as_slice() else {
        return Err(format!("合成器有 {} 块屏", layout.outputs.len()));
    };
    let [screen] = screens else {
        return Err(format!("被控端有 {} 块屏", screens.len()));
    };
    // 旋转看合成器里这块输出自己的，不看 `screen`：屏幕和输出是按名字配对的，
    // XWayland 起的名字和 `wl_output` 的对不上时，屏幕那边就只剩默认的不旋转。
    if output.rotation != Rotation::Normal {
        return Err(format!("屏幕转过 {:?}", output.rotation));
    }
    Ok(Route::Touchscreen {
        rotation: output.rotation,
        width: f64::from(screen.width).max(1.0),
    })
}

impl Engine {
    /// `hardware`, `origin` and `privileged` describe a Windows session; the X
    /// session injector is the same for every caller.
    pub fn new(
        _hardware: bool,
        policy: super::wire::Policy,
        _origin: u32,
        _privileged: bool,
    ) -> Result<Self> {
        let now = Instant::now();
        Ok(Self {
            injector: Injector::open()?,
            keys: BTreeSet::new(),
            parked: BTreeSet::new(),
            buttons: 0,
            touch: None,
            wheel: [0; 2],
            geometry: Default::default(),
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
            mobile: matches!(policy, super::wire::Policy::Mobile),
            configuration: Default::default(),
            foreground: None,
            mouse_policy: Default::default(),
            aim_space: None,
            touch_route: None,
            touch_space: None,
            aimed: false,
        })
    }
    pub fn backend(&self) -> &'static str {
        "X11 XTest · 当前用户"
    }
    /// Ctrl+Alt+Del is an ordinary key chord to an X session, so nothing is
    /// ever withheld for a secure-attention path.
    pub fn take_sas(&mut self) -> bool {
        false
    }
    /// 请合成器重新判定一次指针底下是谁：把指针在原地重报一次。Wayland 的拖放接取
    /// 面摆在指针底下，而合成器只在指针真的动过之后才重判（见
    /// `platform::linux::input::realign_pointer`）。
    ///
    /// 只在指针最后一次是经绝对设备摆的位置时才重报：设备轴上的值只在那时与指针
    /// 一致，相对移动之后再写它，指针会被甩回旧位置。被控本机的人在最后一次注入
    /// 之后自己动过鼠标，轴上的值同样会过期，这里看不出来；拖出时被控端一般没人
    /// 碰鼠标，所以不另做处理。
    pub fn realign_pointer(&mut self) -> Result<bool> {
        if !self.aimed {
            return Ok(false);
        }
        self.injector.realign()
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
    /// Apply the server's per-application mouse rule to the focused program,
    /// matched by executable name as on Windows.
    fn observe_foreground(&mut self) {
        let pid = self.injector.foreground();
        if self.foreground == Some(pid) {
            return;
        }
        self.foreground = Some(pid);
        let name = pid
            .and_then(|pid| std::fs::read_link(format!("/proc/{pid}/exe")).ok())
            .and_then(|path| {
                path.file_name()
                    .map(|name| name.to_string_lossy().to_lowercase())
            });
        let mode = name
            .as_ref()
            .and_then(|name| self.configuration.games.get(name))
            .map_or(2, |rule| rule.mode);
        if self.mouse_policy.mode.unwrap_or(2) == mode {
            return;
        }
        let mut restore = None;
        // The pointer is read from X, so it is compared with X's rectangle:
        // the compositor's logical one is a different space.
        if mode == 2
            && let Ok(screen) = self.geometry.screen(None)
            && let Ok(rect) = screen.rect()
            && let Ok((x, y)) = self.injector.pointer()
            && (rect.left..rect.right).contains(&x)
            && (rect.top..rect.bottom).contains(&y)
        {
            restore = Some([
                f64::from(x - rect.left) / f64::from(screen.width),
                f64::from(y - rect.top) / f64::from(screen.height),
            ]);
        }
        self.mouse_policy = super::config::MousePolicy {
            mode: Some(mode),
            restore,
        };
    }

    /// The compositor's layout bounds when absolute positions can be aimed in
    /// it, or `None` to aim in X's rectangles. A change of answer is logged.
    fn compositor_bounds(&mut self) -> Option<native::Desktop> {
        if !self.injector.has_absolute_pointer() {
            return None;
        }
        let layout = crate::platform::display::compositor_layout();
        let chosen = compositor_bounds(&self.geometry.screens, layout.as_deref());
        let state = chosen.as_ref().map(|_| ()).map_err(Clone::clone);
        if self.aim_space.as_ref() != Some(&state) {
            match &state {
                Ok(()) => tracing::info!("绝对定位按合成器布局（xdg-output）解算"),
                Err(reason) => tracing::warn!(%reason, "绝对定位改用按指针位置换算的相对移动"),
            }
            self.aim_space = Some(state);
        }
        chosen.ok()
    }

    pub fn synchronize(&mut self, geometry: Geometry) -> Result<()> {
        if geometry != self.geometry {
            self.release()?;
            self.geometry = geometry;
        }
        self.observe_foreground();
        Ok(())
    }
    fn key(&mut self, key: u16, down: bool) -> Result<()> {
        if down {
            self.injector.key(key, true)?;
            self.parked.remove(&key);
            self.keys.insert(key);
        } else if self.keys.remove(&key) {
            if let Err(error) = self.injector.key(key, false) {
                self.keys.insert(key);
                return Err(error);
            }
        } else {
            self.parked.remove(&key);
        }
        Ok(())
    }
    fn button(&mut self, button: u8, down: bool) -> Result<()> {
        let (_, bit, native) = BUTTONS
            .into_iter()
            .find(|(wire, _, _)| *wire == button)
            .ok_or_else(|| anyhow::anyhow!("无效鼠标按键"))?;
        if !down && self.buttons & bit == 0 {
            return Ok(());
        }
        self.injector.button(native, down)?;
        if down {
            self.buttons |= bit;
        } else {
            self.buttons &= !bit;
        }
        Ok(())
    }
    fn absolute(&mut self, screen: Option<i32>, x: f64, y: f64) -> Result<()> {
        let screen = self.geometry.screen(screen)?;
        let (logical, rect) = (screen.logical, screen.rect());
        // Every screen in the compositor's space, or every screen in X's:
        // a position named in one and resolved in the other lands off by the
        // output's scale.
        if let Some(bounds) = self.compositor_bounds()
            && let Some(logical) = logical
        {
            let (px, py) = point_in(logical, x, y);
            let result = self.injector.aim(px, py, bounds);
            self.aimed = result.is_ok();
            return result;
        }
        self.aimed = false;
        let (px, py) = point_in(rect?, x, y);
        self.injector.move_to(px, py)
    }
    fn wheel(&mut self, horizontal: bool, delta: i32) -> Result<()> {
        // 120 units per notch as on Windows. X wheels only click, so partial
        // (touchpad) increments accumulate until they make a whole notch.
        let slot = &mut self.wheel[usize::from(horizontal)];
        if (*slot > 0) != (delta > 0) {
            *slot = 0;
        }
        *slot = slot.saturating_add(delta);
        while slot.unsigned_abs() >= 120 {
            let positive = *slot > 0;
            self.injector.wheel_notch(horizontal, positive)?;
            *slot -= if positive { 120 } else { -120 };
        }
        Ok(())
    }
    /// 触摸。合成的触摸屏设备才是这些触点该去的地方：合成器拿到的是一个真正的
    /// 多点设备，触点有几个就是几个，双指滚动、捏合、手势都是真的，而不是把一个
    /// 触点翻成鼠标左键。
    ///
    /// 走哪条路在一串触摸开始时定好、整串都走它：中途换路会让已经按下的触点在原来
    /// 那条路上没人抬起。设备那条路有前提（合成器必然把触摸屏认到唯一那块屏上，见
    /// `route_for`），不满足就整串走单指模拟——模拟走绝对指针，落点总是对的。
    ///
    /// 没有设备时（Xorg 会话，或者 Wayland 下没有 `/dev/uinput` 写权限）在选路时
    /// 就定成模拟。只有设备在一串触摸中途写入失败被丢掉时，`deliver_touch` 才会
    /// 返回 false，这一串余下的触点改走模拟：设备移除时合成器已经把它上面的触点
    /// 全部收尾，模拟从下一个触点重新按下。
    fn touch(&mut self, kind: u8, points: &[super::wire::Point]) -> Result<()> {
        let route = match self.touch_route {
            Some(route) => route,
            None => {
                let route = self.choose_route();
                self.touch_route = Some(route);
                route
            }
        };
        // 3 是抬起、4 是取消：这一串到此为止，下一串重新选路。
        if kind >= 3 {
            self.touch_route = None;
        }
        if let Route::Touchscreen { rotation, width } = route
            && self.deliver_touch(kind, points, rotation, width)?
        {
            // 触摸屏不经绝对指针，可合成器会让光标跟着触点走（sway 在触点底下的
            // 表面不收触摸时就拿它模拟指针），绝对指针轴上那个位置也就过期了。
            self.aimed = false;
            return Ok(());
        }
        if kind == 1 || kind >= 3 {
            if self.touch.take().is_some() {
                self.button(1, false)?;
            }
            if kind >= 3 {
                return Ok(());
            }
        }
        let primary = match self.touch {
            Some(id) => points.iter().find(|p| p.id == id),
            None => points.first(),
        };
        let Some(point) = primary else {
            if self.touch.take().is_some() {
                self.button(1, false)?;
            }
            return Ok(());
        };
        self.absolute(None, point.x, point.y)?;
        if self.touch.is_none() {
            self.button(1, true)?;
            self.touch = Some(point.id);
        }
        Ok(())
    }
    /// 选这一串触摸走哪条路，判定变化时记一条日志（和 `aim_space` 一样只记一次）。
    fn choose_route(&mut self) -> Route {
        let decision = if self.injector.has_touchscreen() {
            route_for(
                crate::platform::display::compositor_layout().as_deref(),
                &self.geometry.screens,
            )
        } else {
            Err("没有虚拟触摸屏".into())
        };
        let state = decision.as_ref().map(|_| ()).map_err(Clone::clone);
        if self.touch_space.as_ref() != Some(&state) {
            match &state {
                Ok(()) => tracing::info!("触摸交给虚拟多点触摸屏"),
                Err(reason) => tracing::info!(%reason, "触摸退回单指指针模拟"),
            }
            self.touch_space = Some(state);
        }
        decision.unwrap_or(Route::Simulated)
    }
    /// 把触点交给触摸屏设备。旋转与屏幕宽度由 `route_for` 在选路时算好带过来，这一
    /// 帧不再查屏幕。返回 false 表示设备已经在这一串中途被丢掉，调用方退回模拟。
    fn deliver_touch(
        &mut self,
        kind: u8,
        points: &[super::wire::Point],
        rotation: Rotation,
        width: f64,
    ) -> Result<bool> {
        let touches = points
            .iter()
            .map(|point| native::Touch {
                id: point.id,
                x: point.x as f32,
                y: point.y as f32,
                // 接触椭圆的长轴半径，按屏幕宽度换算成触摸屏的量程。libinput 处理
                // 触摸屏的那条路径不读这个轴（`ABS_MT_TOUCH_MAJOR`），公开接口里也
                // 没有接触面积，所以报了也没有人看得见；留着只是照实体设备的样子
                // 报一个值。旋转过的屏上两条轴的半径混在一起，取平均。
                radius: (((point.radius_x + point.radius_y) / 2.0) / width) as f32,
            })
            .collect::<Vec<_>>();
        self.injector.touch(kind, &touches, rotation)
    }

    fn chord(&mut self, keys: &[u16], permitted: &impl Fn() -> bool) -> Result<()> {
        for &key in keys {
            ensure!(permitted(), "快捷输入已撤销");
            self.key(key, true)?;
        }
        for &key in keys.iter().rev() {
            self.key(key, false)?;
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
        let flushed = self.injector.flush();
        result.and(flushed)
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
                match command {
                    // Show the desktop / the window overview with the Super
                    // shortcuts GNOME and KDE bind by default.
                    Command::Desktop => self.chord(&[0x5b, 0x44], permitted)?,
                    Command::TaskView => self.chord(&[0x5b], permitted)?,
                    Command::TaskManager => native::system_monitor()?,
                    Command::Lock => native::lock_session()?,
                }
            }
            Event::Heartbeat => {}
            Event::Key {
                key,
                down,
                interruptible,
                toggle,
            } => {
                if down && key == 0x4c && [0x5b, 0x5c].iter().any(|k| self.keys.contains(k)) {
                    // Win+L locks the session on Windows; do the same here.
                    self.release()?;
                    ensure!(permitted(), "被控输入许可已撤销");
                    return native::lock_session();
                }
                self.last_key = self.last_input;
                self.interruptible = interruptible && self.configuration.keyboard_interrupts;
                self.interrupted = false;
                self.backspace = key == 8 && down;
                match (toggle, Lock::from_virtual_key(key)) {
                    (Some(wanted), Some(lock)) => {
                        if !down && self.injector.toggled(lock)? != wanted {
                            self.key(key, true)?;
                            self.key(key, false)?;
                        }
                    }
                    _ => self.key(key, down)?,
                }
            }
            Event::Button { button, down } => self.button(button, down)?,
            Event::Click { button } => {
                self.button(button, true)?;
                self.button(button, false)?;
            }
            Event::Relative { x, y } => {
                self.aimed = false;
                ensure!(
                    x.unsigned_abs() <= 1_000_000 && y.unsigned_abs() <= 1_000_000,
                    "相对位移过大"
                );
                self.injector.move_by(x, y)?;
            }
            Event::RelativeScaled { screen, x, y } => {
                self.aimed = false;
                let s = self.geometry.screen(screen)?;
                self.injector.move_by(
                    (f64::from(s.width) * x).round() as i32,
                    (f64::from(s.height) * y).round() as i32,
                )?;
            }
            Event::Absolute { screen, x, y } => self.absolute(screen, x, y)?,
            Event::Wheel { x, y } => {
                let (horizontal, delta) = if x.unsigned_abs() > y.unsigned_abs() {
                    (true, x)
                } else {
                    (false, y)
                };
                if delta != 0 {
                    self.wheel(horizontal, delta)?;
                }
            }
            Event::Text(text) => self.injector.text(&text, permitted)?,
            Event::Combination { down, keys } => {
                for item in keys {
                    ensure!(permitted(), "组合输入已取消");
                    match item {
                        KeyOrButton::Key(k) => self.key(k, down)?,
                        KeyOrButton::Button(b) => self.button(b, down)?,
                    }
                }
            }
            Event::Touch { kind, points } => self.touch(kind, &points)?,
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
        self.injector.flush()
    }
    pub fn release(&mut self) -> Result<()> {
        let mut failure = self.neutral_keys(true).err();
        for (button, bit, _) in BUTTONS {
            if self.buttons & bit != 0
                && let Err(e) = self.button(button, false)
            {
                failure = Some(e);
            }
        }
        // 触摸屏上的触点也要抬起来，否则合成器会一直以为有一根手指按着。
        if let Err(e) = self.injector.release_touch() {
            failure = Some(e);
        }
        self.touch = None;
        self.touch_route = None;
        self.wheel = [0; 2];
        self.interruptible = false;
        self.backspace = false;
        self.injector.forget_aim();
        self.aimed = false;
        let flushed = self.injector.flush();
        match failure {
            Some(e) => Err(e),
            None => flushed,
        }
    }
    /// Lift every held key. Unless `clear`, the keys stay parked as the
    /// controller's, so its eventual release is absorbed instead of repeated.
    fn neutral_keys(&mut self, clear: bool) -> Result<()> {
        let mut failure = None;
        for key in self.keys.clone() {
            match self.key(key, false) {
                Ok(()) if !clear => {
                    self.parked.insert(key);
                }
                Ok(()) => {}
                Err(e) => failure = Some(e),
            }
        }
        if clear {
            self.parked.clear();
        }
        match failure {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }
}
impl Drop for Engine {
    fn drop(&mut self) {
        let _ = self.release();
    }
}

/// A position given as a fraction of `rect`, on a whole pixel inside it.
fn point_in(rect: Rect, x: f64, y: f64) -> (i32, i32) {
    let px = (f64::from(rect.left) + f64::from(rect.right - rect.left) * x)
        .round()
        .clamp(f64::from(rect.left), f64::from(rect.right - 1));
    let py = (f64::from(rect.top) + f64::from(rect.bottom - rect.top) * y)
        .round()
        .clamp(f64::from(rect.top), f64::from(rect.bottom - 1));
    (px as i32, py as i32)
}

/// The bounds an absolute pointer covers, when every screen has its place in
/// the compositor's layout and lies inside it; otherwise why not. All or
/// nothing: one screen aimed in X's rectangles beside others aimed in the
/// compositor's would mix two spaces.
fn compositor_bounds(
    screens: &[Screen],
    layout: Option<&crate::platform::CompositorLayout>,
) -> Result<native::Desktop, String> {
    let layout = layout.ok_or("合成器没有提供输出布局（xdg-output）")?;
    let bounds = native::layout_bounds(layout).ok_or("合成器布局里没有输出")?;
    if screens.is_empty() {
        return Err("没有可定位的屏幕".into());
    }
    let (left, top) = (i64::from(bounds.left), i64::from(bounds.top));
    let (right, bottom) = (
        left + i64::from(bounds.width),
        top + i64::from(bounds.height),
    );
    for screen in screens {
        let Some(logical) = screen.logical else {
            return Err(format!("屏幕 {} 在合成器布局里没有同名输出", screen.id));
        };
        let inside = logical.left < logical.right
            && logical.top < logical.bottom
            && i64::from(logical.left) >= left
            && i64::from(logical.top) >= top
            && i64::from(logical.right) <= right
            && i64::from(logical.bottom) <= bottom;
        if !inside {
            return Err(format!("屏幕 {} 的合成器位置不在布局范围内", screen.id));
        }
    }
    Ok(bounds)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::platform::{CompositorLayout, CompositorOutput};

    fn output(
        name: Option<&str>,
        left: i32,
        top: i32,
        width: u32,
        height: u32,
    ) -> CompositorOutput {
        CompositorOutput {
            name: name.map(Into::into),
            left,
            top,
            width,
            height,
            rotation: crate::platform::Rotation::default(),
        }
    }

    fn screen(id: i32, x11: (i32, i32, u32, u32), logical: Option<&CompositorOutput>) -> Screen {
        Screen {
            id,
            left: x11.0,
            top: x11.1,
            width: x11.2,
            height: x11.3,
            logical: logical.map(|o| Rect {
                left: o.left,
                top: o.top,
                right: o.left + o.width as i32,
                bottom: o.top + o.height as i32,
            }),
        }
    }

    /// The layout from the PR description: DP-3 at 1.6x rotated, DP-1 at 1.35x.
    fn scaled() -> CompositorLayout {
        CompositorLayout {
            outputs: vec![
                output(Some("DP-3"), 0, 0, 1000, 1600),
                output(Some("DP-1"), 1000, 0, 3793, 1600),
            ],
        }
    }

    #[test]
    fn a_single_unrotated_screen_takes_the_touchscreen() {
        let layout = CompositorLayout {
            outputs: vec![output(Some("DP-1"), 0, 0, 1920, 1080)],
        };
        let screens = vec![screen(0, (0, 0, 1920, 1080), layout.outputs.first())];
        assert_eq!(
            route_for(Some(&layout), &screens),
            Ok(Route::Touchscreen {
                rotation: Rotation::Normal,
                width: 1920.0
            })
        );
    }

    /// 多屏、旋转、读不到布局，一律退回单指模拟：合成器怎么把触摸屏认到哪块屏上各
    /// 家都不一样，猜错就是主控看着这块屏、点下去落在另一块屏上，比单指模拟更差
    /// （模拟走绝对指针，落点总是对的）。
    #[test]
    fn anything_but_a_single_unrotated_screen_falls_back_to_simulation() {
        let single = CompositorLayout {
            outputs: vec![output(Some("DP-1"), 0, 0, 1920, 1080)],
        };
        let one = vec![screen(0, (0, 0, 1920, 1080), single.outputs.first())];
        assert!(route_for(None, &one).is_err(), "没有布局");
        assert!(route_for(Some(&scaled()), &one).is_err(), "合成器两块屏");

        let mut two = one.clone();
        two.push(screen(1, (0, 0, 1920, 1080), single.outputs.first()));
        assert!(route_for(Some(&single), &two).is_err(), "被控端两块屏");

        let mut turned = single.clone();
        turned.outputs[0].rotation = Rotation::Rotate90;
        assert!(route_for(Some(&turned), &one).is_err(), "屏幕转过 90 度");
    }

    /// 旋转只看合成器那块输出：屏幕的名字和 `wl_output` 的没配上时，屏幕这边看不出
    /// 输出转过，也照样退回模拟。
    #[test]
    fn a_rotated_output_falls_back_even_when_the_screen_is_unpaired() {
        let mut layout = CompositorLayout {
            outputs: vec![output(Some("DP-1"), 0, 0, 1080, 1920)],
        };
        layout.outputs[0].rotation = Rotation::Rotate90;
        let unpaired = vec![screen(0, (0, 0, 1080, 1920), None)];
        assert!(unpaired[0].logical.is_none());
        let error = route_for(Some(&layout), &unpaired).unwrap_err();
        assert!(error.contains("Rotate90"), "{error}");
    }

    #[test]
    fn every_screen_paired_aims_in_the_compositor_layout() {
        let layout = scaled();
        let screens = [
            screen(1, (0, 0, 1600, 2560), layout.output("DP-3")),
            screen(2, (1000, 0, 5120, 2160), layout.output("DP-1")),
        ];
        let bounds = compositor_bounds(&screens, Some(&layout)).unwrap();
        assert_eq!(
            bounds,
            native::Desktop {
                left: 0,
                top: 0,
                width: 4793,
                height: 1600
            }
        );
        // The rotated output is named by its logical rectangle alone: the
        // centre of DP-3 is the centre of 1000x1600, not of X's 1600x2560.
        let logical = screens[0].logical.unwrap();
        assert_eq!(point_in(logical, 0.5, 0.5), (500, 800));
        assert_eq!(point_in(logical, 1.0, 1.0), (999, 1599));
    }

    #[test]
    fn one_unpaired_screen_falls_back_for_all() {
        let layout = scaled();
        let screens = [
            screen(1, (0, 0, 1600, 2560), layout.output("DP-3")),
            screen(2, (1000, 0, 5120, 2160), None),
        ];
        let error = compositor_bounds(&screens, Some(&layout)).unwrap_err();
        assert!(error.contains("屏幕 2"), "{error}");
        assert!(compositor_bounds(&screens[..1], None).is_err());
        assert!(compositor_bounds(&[], Some(&layout)).is_err());
    }

    #[test]
    fn a_screen_outside_the_layout_is_refused() {
        let layout = scaled();
        let stray = output(Some("HDMI-1"), 5000, 0, 1920, 1080);
        let screens = [
            screen(1, (0, 0, 1600, 2560), layout.output("DP-3")),
            screen(2, (5000, 0, 1920, 1080), Some(&stray)),
        ];
        assert!(compositor_bounds(&screens, Some(&layout)).is_err());
    }

    #[test]
    fn unnamed_outputs_and_negative_origins_widen_the_bounds() {
        let layout = CompositorLayout {
            outputs: vec![
                output(Some("eDP-1"), 0, 0, 1280, 800),
                output(None, -1920, -200, 1920, 1080),
            ],
        };
        let screens = [screen(1, (0, 0, 2560, 1600), layout.output("eDP-1"))];
        assert_eq!(
            compositor_bounds(&screens, Some(&layout)).unwrap(),
            native::Desktop {
                left: -1920,
                top: -200,
                width: 3200,
                height: 1080
            }
        );
    }
}
