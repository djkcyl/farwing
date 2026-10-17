//! Controlled-session input injection on Linux.
//!
//! Events are injected through the X Test extension, which needs no device
//! permissions inside the user's own Xorg session. Keys travel by physical
//! position: a Windows virtual-key code is mapped to its evdev scan position
//! and from there to the X keycode (evdev + 8 under the evdev and libinput
//! drivers), so the local layout decides the character, as it would for a
//! keyboard plugged into this machine.
//!
//! In a Wayland session XTest only reaches XWayland clients, so there keys,
//! buttons and motion go through a uinput device, as Sunshine does (it needs
//! write access to `/dev/uinput`, granted by the udev rule the packages
//! ship), and text through `zwp_virtual_keyboard_v1`. X11 still reads the key
//! map, the pointer and the lock keys. Xorg sessions keep XTest alone, which
//! needs no setup and positions the pointer absolutely. An absolute position
//! in a Wayland session goes to a second, absolute uinput pointer that the
//! compositor resolves in its own layout (xdg-output); without that layout it
//! becomes a relative move from the current pointer position.
use crate::platform::Rotation;
use anyhow::{Context, Result, bail, ensure};
use std::time::Duration;
use x11rb::connection::{Connection, RequestConnection as _};
use x11rb::protocol::xproto::{AtomEnum, ConnectionExt as _, Keycode, Keysym, Window};
use x11rb::protocol::xtest::ConnectionExt as _;
use x11rb::rust_connection::RustConnection;

const KEY_PRESS: u8 = 2;
const KEY_RELEASE: u8 = 3;
const BUTTON_PRESS: u8 = 4;
const BUTTON_RELEASE: u8 = 5;
const MOTION_NOTIFY: u8 = 6;
const NO_SYMBOL: Keysym = 0;
/// The pause after each typed character.
const TEXT_PACE: Duration = Duration::from_millis(3);

/// Lock keys whose state a controller can ask to match.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Lock {
    Caps,
    Num,
    Scroll,
}
impl Lock {
    pub fn from_virtual_key(key: u16) -> Option<Self> {
        match key {
            0x14 => Some(Self::Caps),
            0x90 => Some(Self::Num),
            0x91 => Some(Self::Scroll),
            _ => None,
        }
    }
}

/// 控制器报来的一个触点：它的 id、屏幕上的比例位置和比例半径。
///
/// 平台层不认识 UU 的消息，所以输入引擎先把触点换算成比例；角度和压力在 evdev
/// 这边没有对应的轴，虚拟触摸屏只报位置和触点直径（见 `ABS_MT_TOUCH_MAJOR`）。
#[derive(Clone, Copy)]
pub(crate) struct Touch {
    pub id: u32,
    pub x: f32,
    pub y: f32,
    pub radius: f32,
}

/// uinput 注入后端。
///
/// Wayland 会话下 XTest 只能影响 XWayland 客户端，Hyprland 等合成器里的
/// 原生客户端收不到；Wayland 会话有 `/dev/uinput` 时改用它注入。X11 仍保留
/// 用于读取键位表、指针位置与锁定键状态。
///
/// 绝对定位另有一台带 `ABS_X`/`ABS_Y` 的设备（Sunshine 的做法），轴范围固定，
/// 由合成器按它自己全局布局的外接矩形解算：不经过 X，也就没有 XWayland 那套
/// 尺寸与缩放。
///
/// 触摸另有一台多点触摸屏设备：合成器把它当成躺在某一块面板上的直接设备，触点由
/// 合成器自己派发给应用——双指滚动、捏合和手势都是真的，而不是把一个触点翻成
/// 鼠标左键。
mod uinput {
    use super::Touch;
    use crate::platform::Rotation;
    use anyhow::{Context, Result, ensure};
    use std::collections::BTreeMap;
    use std::fs::{File, OpenOptions};
    use std::io::Write;
    use std::os::unix::io::AsRawFd;

    const EV_SYN: u16 = 0x00;
    const EV_KEY: u16 = 0x01;
    const EV_REL: u16 = 0x02;
    const EV_ABS: u16 = 0x03;

    const REL_X: u16 = 0x00;
    const REL_Y: u16 = 0x01;
    const REL_HWHEEL: u16 = 0x06;
    const REL_WHEEL: u16 = 0x08;
    const ABS_X: u16 = 0x00;
    const ABS_Y: u16 = 0x01;
    const ABS_MT_SLOT: u16 = 0x2f;
    const ABS_MT_TOUCH_MAJOR: u16 = 0x30;
    const ABS_MT_POSITION_X: u16 = 0x35;
    const ABS_MT_POSITION_Y: u16 = 0x36;
    const ABS_MT_TRACKING_ID: u16 = 0x39;
    const SYN_REPORT: u16 = 0x00;
    /// 绝对轴只受一根指针控制，而不是某一块屏上的坐标。
    const INPUT_PROP_POINTER: u32 = 0;
    /// 坐标落在某一块输出上，是按在面板上的，而不是像鼠标那样在整张桌子上。
    const INPUT_PROP_DIRECT: u32 = 1;
    /// 绝对轴的最大值，与 Sunshine 一样固定，不随布局变：布局变了只改换算，
    /// 不用换设备（换设备又要等合成器重新接手）。
    pub(crate) const AXIS_MAX: i32 = 65535;

    const BTN_LEFT: u16 = 0x110;
    const BTN_RIGHT: u16 = 0x111;
    const BTN_MIDDLE: u16 = 0x112;
    const BTN_SIDE: u16 = 0x113;
    const BTN_EXTRA: u16 = 0x114;
    const BTN_TOUCH: u16 = 0x14a;

    /// 触摸屏两条轴能取的最大值。触摸报的是屏幕上的比例，所以量程只决定精度：
    /// 这个量程在 5120 宽的屏上一个单位是 0.16 个像素。
    const TOUCH_RANGE: f32 = 32767.0;
    /// 触摸屏自称的分辨率（单位/毫米），决定 libinput 和合成器算出来的面板物理尺寸：
    /// 配上这个量程是一块约 819×819 mm 的正方形，不像任何一块真的屏。按物理尺寸
    /// 匹配显示器的合成器（mutter 要求误差在 5% 以内）永远不会因为尺寸把它认到
    /// 某一块屏上。
    const TOUCH_RESOLUTION: i32 = 40;
    /// 触摸屏同时能按住的触点数，和协议允许的一样多。
    const TOUCH_SLOTS: usize = 10;
    /// 设备声明的 tracking id 上限。id 在 `0..=TRACKING_MAX` 里回绕，和内核
    /// `input_mt` 一样按位截断，-1 留给「抬起」。
    const TRACKING_MAX: i32 = 0xffff;

    // linux/uinput.h 的 _IOW('U', n, size)；x86、ARM 与 RISC-V 上相同。
    const UI_DEV_CREATE: libc::c_ulong = 0x5501;
    const UI_DEV_DESTROY: libc::c_ulong = 0x5502;
    const UI_DEV_SETUP: libc::c_ulong = 0x405c_5503;
    const UI_SET_EVBIT: libc::c_ulong = 0x4004_5564;
    const UI_SET_KEYBIT: libc::c_ulong = 0x4004_5565;
    const UI_SET_RELBIT: libc::c_ulong = 0x4004_5566;
    const UI_SET_ABSBIT: libc::c_ulong = 0x4004_5567;
    const UI_SET_PROPBIT: libc::c_ulong = 0x4004_556E;
    const UI_ABS_SETUP: libc::c_ulong = 0x401c_5504;

    #[repr(C)]
    struct InputId {
        bustype: u16,
        vendor: u16,
        product: u16,
        version: u16,
    }
    #[repr(C)]
    struct UinputSetup {
        id: InputId,
        name: [libc::c_char; 80],
        ff_effects_max: u32,
    }
    #[repr(C)]
    #[derive(Default)]
    struct InputAbsInfo {
        value: i32,
        minimum: i32,
        maximum: i32,
        fuzz: i32,
        flat: i32,
        resolution: i32,
    }
    #[repr(C)]
    #[derive(Default)]
    struct UinputAbsSetup {
        code: u16,
        pad: u16,
        absinfo: InputAbsInfo,
    }
    #[repr(C)]
    struct InputEvent {
        sec: libc::c_long,
        usec: libc::c_long,
        kind: u16,
        code: u16,
        value: i32,
    }

    pub(crate) struct Device {
        file: File,
    }

    fn set(fd: i32, request: libc::c_ulong, value: libc::c_ulong) -> Result<()> {
        let rc = unsafe { libc::ioctl(fd, request, value) };
        ensure!(
            rc >= 0,
            "uinput ioctl 0x{request:x} 失败：{}",
            std::io::Error::last_os_error()
        );
        Ok(())
    }

    /// 声明一条绝对轴以及它的范围：合成器就是按这个范围把轴上的值解算成
    /// 全局位置的，范围不对落点就整体偏。
    fn set_abs(fd: i32, axis: u16, maximum: i32, resolution: i32) -> Result<()> {
        set(fd, UI_SET_ABSBIT, axis.into())?;
        let setup = UinputAbsSetup {
            code: axis,
            absinfo: InputAbsInfo {
                maximum,
                resolution,
                ..Default::default()
            },
            ..Default::default()
        };
        let rc = unsafe { libc::ioctl(fd, UI_ABS_SETUP, &setup as *const _) };
        ensure!(
            rc >= 0,
            "设置绝对轴范围失败：{}",
            std::io::Error::last_os_error()
        );
        Ok(())
    }

    /// X 按键编号到 evdev 按键码（1 左、2 中、3 右、8 侧、9 扩展）。
    pub(crate) fn button_code(x_button: u8) -> Option<u16> {
        Some(match x_button {
            1 => BTN_LEFT,
            2 => BTN_MIDDLE,
            3 => BTN_RIGHT,
            8 => BTN_SIDE,
            9 => BTN_EXTRA,
            _ => return None,
        })
    }

    impl Device {
        /// 打开 `/dev/uinput` 并注册一台设备，`capabilities` 声明它能报什么。
        fn create(name: &str, capabilities: impl FnOnce(i32) -> Result<()>) -> Result<Self> {
            let file = OpenOptions::new()
                .write(true)
                .open("/dev/uinput")
                .context("打开 /dev/uinput 失败（需要 input 组与 udev 规则）")?;
            let fd = file.as_raw_fd();
            capabilities(fd)?;
            let mut device_name = [0 as libc::c_char; 80];
            for (slot, byte) in device_name.iter_mut().zip(name.bytes()) {
                *slot = byte as libc::c_char;
            }
            let setup = UinputSetup {
                id: InputId {
                    bustype: 0x06, // BUS_VIRTUAL，与 ydotool 一致
                    vendor: 0x2333,
                    product: 0x6666,
                    version: 1,
                },
                name: device_name,
                ff_effects_max: 0,
            };
            let rc = unsafe { libc::ioctl(fd, UI_DEV_SETUP, &setup as *const _) };
            ensure!(
                rc >= 0,
                "注册 uinput 设备失败：{}",
                std::io::Error::last_os_error()
            );
            set(fd, UI_DEV_CREATE, 0)?;
            Ok(Self { file })
        }

        /// 键鼠合一的相对设备，X 与合成器都能用。
        pub(crate) fn open() -> Result<Self> {
            Self::create("OpenUUYC virtual input", |fd| {
                set(fd, UI_SET_EVBIT, EV_KEY.into())?;
                set(fd, UI_SET_EVBIT, EV_REL.into())?;
                // Keyboard keys (every code `evdev` maps to is below 256) and the
                // mouse buttons only. Declaring BTN_TOUCH, BTN_TOOL_* or the
                // joystick range makes udev and libinput classify the device as
                // something else.
                for code in (1..0x100u16).chain(BTN_LEFT..=BTN_EXTRA) {
                    set(fd, UI_SET_KEYBIT, code.into())?;
                }
                for bit in [REL_X, REL_Y, REL_WHEEL, REL_HWHEEL] {
                    set(fd, UI_SET_RELBIT, bit.into())?;
                }
                Ok(())
            })
        }

        /// 绝对指针：`ABS_X`/`ABS_Y` 的范围固定为 `0..=AXIS_MAX`，合成器把它按
        /// 比例映射到自己全部输出的外接矩形上。
        pub(crate) fn open_absolute() -> Result<Self> {
            Self::create("OpenUUYC virtual absolute pointer", |fd| {
                // 按钮也要声明：libinput 靠它认出这是一只鼠标，否则一条只有绝对
                // 轴的设备会被归到触摸屏之类里去。
                set(fd, UI_SET_EVBIT, EV_KEY.into())?;
                for code in BTN_LEFT..=BTN_EXTRA {
                    set(fd, UI_SET_KEYBIT, code.into())?;
                }
                set(fd, UI_SET_EVBIT, EV_ABS.into())?;
                set_abs(fd, ABS_X, AXIS_MAX, 0)?;
                set_abs(fd, ABS_Y, AXIS_MAX, 0)?;
                // 指针而不是“直接设备”：INPUT_PROP_DIRECT 是在说坐标只落在
                // 一块屏上，而这里描述的是整个桌面。
                set(fd, UI_SET_PROPBIT, INPUT_PROP_POINTER.into())
            })
            .context("创建虚拟绝对鼠标失败")
        }

        fn emit(&self, kind: u16, code: u16, value: i32) -> Result<()> {
            let event = InputEvent {
                sec: 0,
                usec: 0,
                kind,
                code,
                value,
            };
            let bytes = unsafe {
                std::slice::from_raw_parts(
                    &event as *const _ as *const u8,
                    std::mem::size_of::<InputEvent>(),
                )
            };
            let mut sink = &self.file;
            sink.write_all(bytes).context("写入 uinput 事件失败")?;
            Ok(())
        }

        fn sync(&self) -> Result<()> {
            self.emit(EV_SYN, SYN_REPORT, 0)
        }

        /// 写一串事件，最后补一个 report：内核只有在一个同步里收到它们，
        /// 才会当成同一时刻的状态（触点协议的顺序要求就落在这一点上）。
        fn send(&self, events: &[(u16, u16, i32)]) -> Result<()> {
            for &(kind, code, value) in events {
                self.emit(kind, code, value)?;
            }
            self.sync()
        }

        pub(crate) fn key(&self, code: u16, down: bool) -> Result<()> {
            self.emit(EV_KEY, code, i32::from(down))?;
            self.sync()
        }

        pub(crate) fn button(&self, x_button: u8, down: bool) -> Result<()> {
            let code = button_code(x_button).context("不支持的鼠标按键")?;
            self.key(code, down)
        }

        pub(crate) fn move_by(&self, dx: i32, dy: i32) -> Result<()> {
            self.emit(EV_REL, REL_X, dx)?;
            self.emit(EV_REL, REL_Y, dy)?;
            self.sync()
        }

        /// 绝对位置，取的是设备轴上的值（`0..=AXIS_MAX`）。
        pub(crate) fn move_absolute(&self, x: i32, y: i32) -> Result<()> {
            self.emit(EV_ABS, ABS_X, x)?;
            self.emit(EV_ABS, ABS_Y, y)?;
            self.sync()
        }

        pub(crate) fn wheel(&self, horizontal: bool, positive: bool) -> Result<()> {
            let (code, value) = match (horizontal, positive) {
                (false, true) => (REL_WHEEL, 1),
                (false, false) => (REL_WHEEL, -1),
                (true, true) => (REL_HWHEEL, 1),
                (true, false) => (REL_HWHEEL, -1),
            };
            self.emit(EV_REL, code, value)?;
            self.sync()
        }
    }

    impl Drop for Device {
        fn drop(&mut self) {
            let _ = unsafe { libc::ioctl(self.file.as_raw_fd(), UI_DEV_DESTROY, 0) };
        }
    }

    // --- the touchscreen ----------------------------------------------------

    /// 一块虚拟多点触摸屏。
    ///
    /// 它比「用指针模拟触摸」多出来的东西，正是合成器愿意认它是一块触摸屏的理由：
    /// `INPUT_PROP_DIRECT` 和多点轴（`ABS_MT_*`）让 libinput 把它判成触摸屏，于是
    /// 它报的触点被合成器原样派发给应用——双指滚动、捏合、手势都是真的，而不是把
    /// 一个触点翻译成鼠标左键。
    ///
    /// 触点大小照实体触摸屏的样子报在 `ABS_MT_TOUCH_MAJOR` 上，但 libinput 处理
    /// 触摸屏的那条路径不读这个轴，公开接口里也没有触摸屏的接触面积，所以合成器和
    /// 应用拿不到它。
    ///
    /// 设备坐标是「面板自己」的比例坐标，合成器把它铺到映射给它的那块输出（或者
    /// 整块布局）上，所以屏幕大小和缩放都不进这里。铺的时候套不套输出的旋转各家
    /// 不同：niri 和 KWin 总是套，mutter、wlroots 和 Hyprland 只在用户配了映射或
    /// 变换时才套。所以转过的屏引擎一律不交给它（见 engine 里的 `route_for`）；
    /// `panel` 是「总是套旋转」那一类合成器的逆，留给以后按合成器逐个放开。
    ///
    /// 只在 Wayland 会话下建（`Injector::open` 里跟着 uinput 设备走）。Xorg 的
    /// libinput 驱动会把一块没有映射的触摸屏铺满整个 X screen，也就是横跨所有
    /// 显示器，所以 Xorg 被控端不建它，触摸仍由 XTest 模拟；要在 Xorg 上打开
    /// uinput，先想好这块屏的映射。
    pub(crate) struct Touchscreen {
        device: Device,
        contacts: Contacts,
    }

    impl Touchscreen {
        pub(crate) fn create() -> Result<Self> {
            let device = Device::create("OpenUUYC 虚拟触摸屏", |fd| {
                set(fd, UI_SET_EVBIT, EV_KEY.into())?;
                set(fd, UI_SET_KEYBIT, BTN_TOUCH.into())?;
                set(fd, UI_SET_EVBIT, EV_ABS.into())?;
                for axis in [ABS_X, ABS_Y, ABS_MT_POSITION_X, ABS_MT_POSITION_Y] {
                    set_abs(fd, axis, TOUCH_RANGE as i32, TOUCH_RESOLUTION)?;
                }
                set_abs(fd, ABS_MT_SLOT, TOUCH_SLOTS as i32 - 1, 0)?;
                set_abs(fd, ABS_MT_TRACKING_ID, TRACKING_MAX, 0)?;
                // 接触椭圆的长轴，按 evdev 的定义是直径，量程和位置轴一样。
                set_abs(fd, ABS_MT_TOUCH_MAJOR, TOUCH_RANGE as i32, 0)?;
                set(fd, UI_SET_PROPBIT, INPUT_PROP_DIRECT.into())
            })
            .context("创建虚拟触摸屏失败")?;
            Ok(Self {
                device,
                contacts: Contacts::default(),
            })
        }

        /// 算出这一帧触点要写进设备的帧。触点比槽位多时报错，什么也不改，设备
        /// 照样能用。
        pub(crate) fn step(
            &mut self,
            kind: u8,
            points: &[Touch],
            rotation: Rotation,
        ) -> Result<Vec<Frame>> {
            self.contacts.step(kind, points, rotation)
        }

        /// 把帧写进设备。写失败时设备里按着哪些触点就不知道了，调用方要丢掉这块
        /// 触摸屏：设备一移除，合成器（libinput）会自己把它上面的触点全部收尾。
        pub(crate) fn write(&self, frames: &[Frame]) -> Result<()> {
            for frame in frames {
                self.device.send(frame)?;
            }
            Ok(())
        }

        /// 抬起所有触点。和 `write` 一样，失败了这块触摸屏就不能再用。
        pub(crate) fn release(&mut self) -> Result<()> {
            let frames = self.contacts.lift_all();
            self.write(&frames)
        }
    }

    /// 触摸屏上的触点：控制器给的 id 占着哪个槽位、按没按着。
    ///
    /// 这段逻辑和设备本身分开，是为了能脱开 `/dev/uinput` 单独验：触摸屏的协议要求
    /// 全在帧的顺序里（谁先抬起、槽位什么时候能复用、BTN_TOUCH 什么时候翻转）。
    #[derive(Clone, Default)]
    struct Contacts {
        active: BTreeMap<u32, Contact>,
        used: [bool; TOUCH_SLOTS],
        /// 上一个发出去的 tracking id，每个新触点加一，在声明的范围里回绕。
        tracking: i32,
        /// 上一次发出去的按下状态，只在变化时重发 BTN_TOUCH。
        touching: bool,
    }

    #[derive(Clone)]
    struct Contact {
        slot: u8,
        tracking: i32,
    }

    /// 送进设备的一帧：一组事件，内核把它们当成一次同步。
    pub(crate) type Frame = Vec<(u16, u16, i32)>;

    impl Contacts {
        /// 一帧触点。`kind` 是 UU 的说法：1 是「触点集合从现在开始」，2 是移动，
        /// 3 是抬起，4 是取消。返回要写进设备的帧。
        ///
        /// 一次同步里不能把「抬起旧触点」和「按下新触点」当成同一批：新触点常常拿到
        /// 刚空出来的槽位，同一帧里同一个槽位先写 -1 再写新 id，libinput
        /// （`evdev-fallback.c`）会当成内核的错误，抬起和按下一起丢掉。所以抬起
        /// 总是自己一帧，和 Windows 的 `Touch::update` 先单独注入一帧 UP 一样。
        ///
        /// 出错时（触点比槽位多）什么也不改：整步先在一份副本上算完，成功了才换上。
        /// 否则消失的触点已经从这里删掉，抬起它们的那一帧却没有发出去，合成器那边
        /// 就会一直按着。
        fn step(&mut self, kind: u8, points: &[Touch], rotation: Rotation) -> Result<Vec<Frame>> {
            let mut next = self.clone();
            let frames = next.advance(kind, points, rotation)?;
            *self = next;
            Ok(frames)
        }

        fn advance(
            &mut self,
            kind: u8,
            points: &[Touch],
            rotation: Rotation,
        ) -> Result<Vec<Frame>> {
            let mut frames = Vec::new();
            if kind == 1 || kind >= 3 {
                frames.extend(self.lift_all());
                if kind >= 3 {
                    // 抬起和取消在 evdev 里没有区别：一个触点只有抬起这一种结束。
                    return Ok(frames);
                }
            }
            // 这一帧里没有了的触点先抬起，槽位空出来给新触点用。
            let gone: Vec<u32> = self
                .active
                .keys()
                .copied()
                .filter(|id| !points.iter().any(|point| point.id == *id))
                .collect();
            let mut lifts = Frame::new();
            for id in gone {
                let Some(contact) = self.active.remove(&id) else {
                    continue;
                };
                self.used[usize::from(contact.slot)] = false;
                lifts.push((EV_ABS, ABS_MT_SLOT, i32::from(contact.slot)));
                lifts.push((EV_ABS, ABS_MT_TRACKING_ID, -1));
            }
            if !lifts.is_empty() {
                if self.active.is_empty() && self.touching {
                    self.touching = false;
                    lifts.push((EV_KEY, BTN_TOUCH, 0));
                }
                frames.push(lifts);
            }
            let mut frame = Frame::new();
            for point in points {
                let (x, y) = position(point, rotation);
                match self.active.get(&point.id) {
                    Some(contact) => {
                        frame.push((EV_ABS, ABS_MT_SLOT, i32::from(contact.slot)));
                    }
                    None => {
                        let slot = self
                            .used
                            .iter()
                            .position(|used| !used)
                            .context("虚拟触摸屏的触点已经用满")?;
                        self.used[slot] = true;
                        // 新触点要一个新的 tracking id；抬起时把那个槽位的 id 写回 -1。
                        let contact = Contact {
                            slot: slot as u8,
                            tracking: self.next_tracking(),
                        };
                        frame.push((EV_ABS, ABS_MT_SLOT, i32::from(contact.slot)));
                        frame.push((EV_ABS, ABS_MT_TRACKING_ID, contact.tracking));
                        self.active.insert(point.id, contact);
                    }
                }
                frame.push((EV_ABS, ABS_MT_POSITION_X, x));
                frame.push((EV_ABS, ABS_MT_POSITION_Y, y));
                frame.push((EV_ABS, ABS_MT_TOUCH_MAJOR, touch_value(2.0 * point.radius)));
            }
            // 第一个触点的单点坐标。实体触摸屏也会报，libinput 的单点回退路径读它。
            if let Some(point) = points.first() {
                let (x, y) = position(point, rotation);
                frame.push((EV_ABS, ABS_X, x));
                frame.push((EV_ABS, ABS_Y, y));
            }
            let touching = !self.active.is_empty();
            if touching != self.touching {
                self.touching = touching;
                frame.push((EV_KEY, BTN_TOUCH, i32::from(touching)));
            }
            if !frame.is_empty() {
                frames.push(frame);
            }
            Ok(frames)
        }

        /// 下一个 tracking id：加一后截到声明的范围里，跳过还按着的触点正在用的 id
        /// （回绕一圈之后才可能撞上；按着的最多 `TOUCH_SLOTS` 个，所以一定找得到）。
        fn next_tracking(&mut self) -> i32 {
            loop {
                self.tracking = (self.tracking + 1) & TRACKING_MAX;
                let tracking = self.tracking;
                if !self
                    .active
                    .values()
                    .any(|contact| contact.tracking == tracking)
                {
                    return tracking;
                }
            }
        }

        /// 抬起所有触点，并把槽位还回去。
        fn lift_all(&mut self) -> Vec<Frame> {
            if self.active.is_empty() {
                return Vec::new();
            }
            let mut frame = Frame::new();
            for contact in std::mem::take(&mut self.active).into_values() {
                frame.push((EV_ABS, ABS_MT_SLOT, i32::from(contact.slot)));
                frame.push((EV_ABS, ABS_MT_TRACKING_ID, -1));
            }
            self.used = [false; TOUCH_SLOTS];
            self.touching = false;
            frame.push((EV_KEY, BTN_TOUCH, 0));
            vec![frame]
        }
    }

    /// 触点在内核里的位置：先把屏幕比例变回面板比例，再放到量程上。
    fn position(point: &Touch, rotation: Rotation) -> (i32, i32) {
        let (x, y) = panel(rotation, point.x.clamp(0.0, 1.0), point.y.clamp(0.0, 1.0));
        (touch_value(x), touch_value(y))
    }

    /// A position on the screen as a fraction of it, to the same position as a
    /// fraction of the touchscreen's own, untransformed panel.
    ///
    /// A compositor that turns touches with the output does the opposite: it
    /// reads a touch as a fraction of the panel and applies the output's
    /// transform to it, in the output's rectangle (smithay's
    /// `transform_point_in`, which niri scales a touch device's axes with;
    /// KWin's `devicePointToGlobalPosition` turns the same way but ignores the
    /// flips). Undoing that is this inverse, in normalized coordinates: both
    /// spaces are the unit square, so the axis lengths, the output's scale and
    /// its physical size never enter.
    fn panel(rotation: Rotation, x: f32, y: f32) -> (f32, f32) {
        match rotation {
            Rotation::Normal => (x, y),
            Rotation::Rotate90 => (y, 1.0 - x),
            Rotation::Rotate180 => (1.0 - x, 1.0 - y),
            Rotation::Rotate270 => (1.0 - y, x),
            Rotation::Flipped => (1.0 - x, y),
            Rotation::Flipped90 => (y, x),
            Rotation::Flipped180 => (x, 1.0 - y),
            Rotation::Flipped270 => (1.0 - y, 1.0 - x),
        }
    }

    /// 一个比例值放到触摸屏的量程上。
    fn touch_value(fraction: f32) -> i32 {
        (fraction.clamp(0.0, 1.0) * TOUCH_RANGE).round() as i32
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        fn contact(id: u32, x: f32, y: f32) -> Touch {
            Touch {
                id,
                x,
                y,
                radius: 0.02,
            }
        }

        /// 触点按下的那一帧：新槽位、新的 tracking id、坐标，以及整个面板的按下状态。
        #[test]
        fn the_first_contact_presses_the_panel_and_lifting_it_releases_it() {
            let mut contacts = Contacts::default();
            let frames = contacts
                .step(2, &[contact(7, 0.25, 0.75)], Rotation::Normal)
                .unwrap();
            assert_eq!(frames.len(), 1);
            let frame = &frames[0];
            assert!(frame.contains(&(EV_ABS, ABS_MT_SLOT, 0)));
            assert!(frame.contains(&(EV_ABS, ABS_MT_TRACKING_ID, 1)));
            assert!(frame.contains(&(EV_ABS, ABS_MT_POSITION_X, touch_value(0.25))));
            assert!(frame.contains(&(EV_ABS, ABS_MT_POSITION_Y, touch_value(0.75))));
            // 触点大小报的是直径：半径 0.02 的触点是 0.04
            assert!(frame.contains(&(EV_ABS, ABS_MT_TOUCH_MAJOR, touch_value(0.04))));
            // 单点坐标跟着第一个触点走
            assert!(frame.contains(&(EV_ABS, ABS_X, touch_value(0.25))));
            assert!(frame.contains(&(EV_ABS, ABS_Y, touch_value(0.75))));
            assert!(frame.contains(&(EV_KEY, BTN_TOUCH, 1)));

            let frames = contacts.step(3, &[], Rotation::Normal).unwrap();
            assert_eq!(frames.len(), 1);
            assert!(frames[0].contains(&(EV_ABS, ABS_MT_TRACKING_ID, -1)));
            assert!(frames[0].contains(&(EV_KEY, BTN_TOUCH, 0)));
            assert!(contacts.active.is_empty());
            assert!(contacts.used.iter().all(|used| !used));
        }

        /// 会按输出旋转解释设备坐标的合成器（niri、KWin）上，转过的屏两条轴要换过来报。
        #[test]
        fn a_rotated_output_moves_the_axes_the_panel_reports() {
            let mut contacts = Contacts::default();
            // 屏幕上的 (0.25, 0.6)：面板逆时针转 90° 之后落在面板的 (0.6, 0.75)
            let frames = contacts
                .step(2, &[contact(1, 0.25, 0.6)], Rotation::Rotate90)
                .unwrap();
            let frame = &frames[0];
            assert!(frame.contains(&(EV_ABS, ABS_MT_POSITION_X, touch_value(0.6))));
            assert!(frame.contains(&(EV_ABS, ABS_MT_POSITION_Y, touch_value(0.75))));
            assert!(!frame.contains(&(EV_ABS, ABS_MT_POSITION_X, touch_value(0.25))));
        }

        /// 两根手指各占一个槽位，抬起其中一根不能碰到另一根，槽位还得还回来。
        #[test]
        fn two_contacts_take_two_slots_and_a_gone_one_frees_only_its_own() {
            let mut contacts = Contacts::default();
            contacts
                .step(
                    2,
                    &[contact(1, 0.1, 0.1), contact(2, 0.9, 0.9)],
                    Rotation::Normal,
                )
                .unwrap();
            assert_eq!(contacts.active.len(), 2);
            assert_eq!(contacts.used.iter().filter(|used| **used).count(), 2);

            // 第二根手指抬起，第一根还在：抬起自己一帧，面板仍然是按下的；剩下那根的
            // 移动在下一帧
            let frames = contacts
                .step(2, &[contact(1, 0.2, 0.2)], Rotation::Normal)
                .unwrap();
            assert_eq!(frames.len(), 2);
            assert_eq!(
                frames[0],
                [(EV_ABS, ABS_MT_SLOT, 1), (EV_ABS, ABS_MT_TRACKING_ID, -1)]
            );
            assert!(!frames[1].contains(&(EV_KEY, BTN_TOUCH, 0)));
            assert!(frames[1].contains(&(EV_ABS, ABS_MT_SLOT, 0)));
            assert!(frames[1].contains(&(EV_ABS, ABS_MT_POSITION_X, touch_value(0.2))));
            assert_eq!(contacts.active.len(), 1);
            assert!(!contacts.used[1]);
            assert!(contacts.used[0]);
        }

        /// 每一帧里每个槽位写过的 tracking id。libinput 在同一帧里看到一个槽位先
        /// 写 -1 再写新 id，会把抬起和按下一起丢掉。
        fn ends_and_begins_in_one_frame(frames: &[Frame]) -> bool {
            frames.iter().any(|frame| {
                let mut slot = None;
                let mut written: BTreeMap<i32, Vec<i32>> = BTreeMap::new();
                for &(kind, code, value) in frame {
                    match (kind, code) {
                        (EV_ABS, ABS_MT_SLOT) => slot = Some(value),
                        (EV_ABS, ABS_MT_TRACKING_ID) => {
                            written.entry(slot.unwrap()).or_default().push(value)
                        }
                        _ => {}
                    }
                }
                written
                    .values()
                    .any(|ids| ids.contains(&-1) && ids.iter().any(|id| *id >= 0))
            })
        }

        /// 一根手指抬起、另一根同时按下（主控在两次点击之间没有发抬起）：抬起和按下
        /// 落在两帧里，按下的那一帧可以复用刚空出来的槽位。
        #[test]
        fn a_contact_replaced_in_one_step_lifts_and_presses_in_separate_frames() {
            let mut contacts = Contacts::default();
            contacts
                .step(2, &[contact(1, 0.5, 0.5)], Rotation::Normal)
                .unwrap();
            let frames = contacts
                .step(2, &[contact(2, 0.3, 0.3)], Rotation::Normal)
                .unwrap();
            assert_eq!(frames.len(), 2);
            // 先抬起，面板也跟着松开
            assert_eq!(
                frames[0],
                [
                    (EV_ABS, ABS_MT_SLOT, 0),
                    (EV_ABS, ABS_MT_TRACKING_ID, -1),
                    (EV_KEY, BTN_TOUCH, 0),
                ]
            );
            // 再按下新触点，同一个槽位、新的 tracking id
            assert!(frames[1].contains(&(EV_ABS, ABS_MT_SLOT, 0)));
            assert!(frames[1].contains(&(EV_ABS, ABS_MT_TRACKING_ID, 2)));
            assert!(!frames[1].contains(&(EV_ABS, ABS_MT_TRACKING_ID, -1)));
            assert!(frames[1].contains(&(EV_KEY, BTN_TOUCH, 1)));
            assert!(!ends_and_begins_in_one_frame(&frames));
        }

        /// 一串换手指的帧里，没有哪一帧对同一个槽位既写 -1 又写新 id。
        #[test]
        fn no_frame_ends_and_begins_a_contact_in_the_same_slot() {
            let mut contacts = Contacts::default();
            let steps: [&[Touch]; 6] = [
                &[contact(1, 0.1, 0.1), contact(2, 0.2, 0.2)],
                // 1 抬起，3 按下：3 拿到 1 空出来的槽位 0
                &[contact(2, 0.2, 0.3), contact(3, 0.4, 0.4)],
                // 两根都换掉
                &[contact(4, 0.5, 0.5), contact(5, 0.6, 0.6)],
                &[contact(5, 0.6, 0.7)],
                &[contact(6, 0.7, 0.7)],
                &[],
            ];
            for points in steps {
                let frames = contacts.step(2, points, Rotation::Normal).unwrap();
                assert!(!ends_and_begins_in_one_frame(&frames), "{frames:?}");
            }
            assert!(contacts.active.is_empty());
            assert!(!contacts.touching);
        }

        /// tracking id 在声明的 `0..=TRACKING_MAX` 里回绕，不会是 -1，也不会撞上
        /// 还按着的触点。
        #[test]
        fn tracking_ids_wrap_within_the_declared_range() {
            let mut contacts = Contacts::default();
            contacts.tracking = TRACKING_MAX - 1;
            contacts
                .step(2, &[contact(1, 0.5, 0.5)], Rotation::Normal)
                .unwrap();
            assert_eq!(contacts.active[&1].tracking, TRACKING_MAX);
            // 1 一直按着；下一个回绕到 0
            let frames = contacts
                .step(
                    2,
                    &[contact(1, 0.5, 0.5), contact(2, 0.6, 0.6)],
                    Rotation::Normal,
                )
                .unwrap();
            assert!(frames[0].contains(&(EV_ABS, ABS_MT_TRACKING_ID, 0)));
            assert_eq!(contacts.active[&2].tracking, 0);

            // 又绕一圈回到 TRACKING_MAX 和 0 时，它们还被 1 和 2 占着，跳过去
            contacts.tracking = TRACKING_MAX - 1;
            contacts
                .step(
                    2,
                    &[
                        contact(1, 0.5, 0.5),
                        contact(2, 0.6, 0.6),
                        contact(3, 0.7, 0.7),
                    ],
                    Rotation::Normal,
                )
                .unwrap();
            assert_eq!(contacts.active[&1].tracking, TRACKING_MAX);
            assert_eq!(contacts.active[&3].tracking, 1);
        }

        /// 「触点集合从现在开始」这一类帧，先抬起旧的一套，再按下新的一套。
        #[test]
        fn a_new_set_lifts_the_old_contacts_in_a_frame_of_their_own() {
            let mut contacts = Contacts::default();
            contacts
                .step(2, &[contact(1, 0.5, 0.5)], Rotation::Normal)
                .unwrap();
            let frames = contacts
                .step(1, &[contact(9, 0.5, 0.5)], Rotation::Normal)
                .unwrap();
            assert_eq!(frames.len(), 2);
            assert!(frames[0].contains(&(EV_KEY, BTN_TOUCH, 0)));
            assert!(frames[1].contains(&(EV_KEY, BTN_TOUCH, 1)));
            assert_eq!(contacts.active.keys().copied().collect::<Vec<_>>(), [9]);
        }

        /// 取消和抬起一样：触点全抬起，再收到一次抬起不必再发一帧。
        #[test]
        fn a_cancelled_set_lifts_every_contact() {
            let mut contacts = Contacts::default();
            contacts
                .step(2, &[contact(1, 0.5, 0.5)], Rotation::Normal)
                .unwrap();
            let frames = contacts
                .step(4, &[contact(1, 0.5, 0.5)], Rotation::Normal)
                .unwrap();
            assert_eq!(frames.len(), 1);
            assert!(frames[0].contains(&(EV_KEY, BTN_TOUCH, 0)));
            assert!(contacts.active.is_empty());
            assert!(contacts.step(3, &[], Rotation::Normal).unwrap().is_empty());
        }

        /// Where a screen position lands on an untransformed panel: on an output
        /// the compositor has turned 90°, the screen's top-left is the panel's
        /// bottom-left, and so on.
        #[test]
        fn a_rotated_output_maps_screen_corners_to_panel_corners() {
            let corners = [(0.0, 0.0), (1.0, 0.0), (0.0, 1.0), (1.0, 1.0)];
            assert_eq!(panel(Rotation::Normal, 0.25, 0.75), (0.25, 0.75));
            assert_eq!(
                corners.map(|(x, y)| panel(Rotation::Rotate90, x, y)),
                [(0.0, 1.0), (0.0, 0.0), (1.0, 1.0), (1.0, 0.0)]
            );
            assert_eq!(
                corners.map(|(x, y)| panel(Rotation::Rotate180, x, y)),
                [(1.0, 1.0), (0.0, 1.0), (1.0, 0.0), (0.0, 0.0)]
            );
            assert_eq!(
                corners.map(|(x, y)| panel(Rotation::Rotate270, x, y)),
                [(1.0, 0.0), (1.0, 1.0), (0.0, 0.0), (0.0, 1.0)]
            );
        }

        /// The compositor's side of the same mapping: a point as a fraction of the
        /// panel, to a point as a fraction of the screen, which is smithay's
        /// `transform_point_in` with the panel's own size as the area. Written out
        /// here so [`panel`] is checked against it rather than against
        /// itself.
        fn compositor(rotation: Rotation, point: (f32, f32)) -> (f32, f32) {
            let (x, y) = point;
            match rotation {
                Rotation::Normal => (x, y),
                Rotation::Rotate90 => (1.0 - y, x),
                Rotation::Rotate180 => (1.0 - x, 1.0 - y),
                Rotation::Rotate270 => (y, 1.0 - x),
                Rotation::Flipped => (1.0 - x, y),
                Rotation::Flipped90 => (y, x),
                Rotation::Flipped180 => (x, 1.0 - y),
                Rotation::Flipped270 => (1.0 - y, 1.0 - x),
            }
        }

        /// Every turn, including the flipped ones: where the compositor puts the
        /// point this side names for it has to be the point the controller aimed
        /// at in the first place.
        #[test]
        fn a_panel_position_is_the_inverse_of_what_the_compositor_does() {
            let rotations = [
                Rotation::Normal,
                Rotation::Rotate90,
                Rotation::Rotate180,
                Rotation::Rotate270,
                Rotation::Flipped,
                Rotation::Flipped90,
                Rotation::Flipped180,
                Rotation::Flipped270,
            ];
            for rotation in rotations {
                for (x, y) in [(0.0, 0.0), (1.0, 0.0), (0.0, 1.0), (1.0, 1.0), (0.25, 0.6)] {
                    let on_panel = panel(rotation, x, y);
                    let (sx, sy) = compositor(rotation, on_panel);
                    assert!(
                        (sx - x).abs() < 1e-6 && (sy - y).abs() < 1e-6,
                        "{rotation:?}: 屏幕 ({x}, {y}) 报成面板 {on_panel:?}，合成器又算回 ({sx}, {sy})"
                    );
                }
            }
        }

        /// 触点数超过槽位时报错，而不是把触点悄悄丢掉或者panic。
        #[test]
        fn the_panel_refuses_more_contacts_than_it_has_slots() {
            let mut contacts = Contacts::default();
            let points: Vec<Touch> = (0..TOUCH_SLOTS as u32 + 1)
                .map(|id| contact(id, 0.5, 0.5))
                .collect();
            assert!(contacts.step(2, &points, Rotation::Normal).is_err());
        }

        /// 走不下去的一步什么也不改：本来按着的触点还记着，之后照样能抬起来。
        #[test]
        fn a_refused_step_keeps_the_contacts_it_had() {
            let mut contacts = Contacts::default();
            contacts
                .step(
                    2,
                    &[contact(1, 0.1, 0.1), contact(2, 0.2, 0.2)],
                    Rotation::Normal,
                )
                .unwrap();
            // 1 和 2 都没了，换上 11 个新触点：槽位不够
            let points: Vec<Touch> = (10..10 + TOUCH_SLOTS as u32 + 1)
                .map(|id| contact(id, 0.5, 0.5))
                .collect();
            assert!(contacts.step(2, &points, Rotation::Normal).is_err());
            assert_eq!(contacts.active.keys().copied().collect::<Vec<_>>(), [1, 2]);
            assert_eq!(contacts.used.iter().filter(|used| **used).count(), 2);
            assert_eq!(contacts.tracking, 2);
            assert!(contacts.touching);

            let frames = contacts.step(3, &[], Rotation::Normal).unwrap();
            assert_eq!(frames.len(), 1);
            assert!(frames[0].contains(&(EV_ABS, ABS_MT_SLOT, 0)));
            assert!(frames[0].contains(&(EV_ABS, ABS_MT_SLOT, 1)));
            assert!(frames[0].contains(&(EV_KEY, BTN_TOUCH, 0)));
        }
    }
}

/// Whether Hyprland's IPC is there to ask; other compositors have no
/// equivalent, and positions then come from XWayland.
fn hyprland() -> bool {
    std::env::var_os("HYPRLAND_INSTANCE_SIGNATURE").is_some()
}

/// 向 Hyprland 的 IPC socket 发一条请求并读回响应。
///
/// XWayland 的 `query_pointer` 只在 XWayland 表面上跟随指针；光标停在原生
/// Wayland 客户端上时它返回的是陈旧值（实测移动到极限后仍然不动），所以
/// 绝对定位必须用合成器的权威数据。
fn hyprland_request(command: &str) -> Option<String> {
    use std::io::{Read as _, Write as _};
    let signature = std::env::var("HYPRLAND_INSTANCE_SIGNATURE").ok()?;
    let runtime = std::env::var("XDG_RUNTIME_DIR").ok()?;
    let mut socket =
        std::os::unix::net::UnixStream::connect(format!("{runtime}/hypr/{signature}/.socket.sock"))
            .ok()?;
    socket
        .set_read_timeout(Some(Duration::from_millis(300)))
        .ok()?;
    socket.write_all(command.as_bytes()).ok()?;
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        match socket.read(&mut chunk) {
            Ok(0) => break,
            Ok(read) => {
                buffer.extend_from_slice(&chunk[..read]);
                if buffer.len() > (1 << 20) {
                    break;
                }
            }
            Err(_) => break,
        }
    }
    String::from_utf8(buffer).ok()
}

/// Hyprland 合成器里的光标位置（逻辑全局布局坐标）。
fn hyprland_cursor() -> Option<(i32, i32)> {
    let reply = hyprland_request("cursorpos")?;
    let (x, y) = reply.trim().split_once(',')?;
    Some((x.trim().parse().ok()?, y.trim().parse().ok()?))
}

/// Hyprland 每个显示器的逻辑矩形：`(名称, left, top, 逻辑宽, 逻辑高)`。
///
/// `hyprctl monitors` 报的是物理分辨率与缩放，光标坐标用的是逻辑尺寸，
/// 所以要按 `scale` 折算。
fn hyprland_monitors() -> Option<Vec<(String, i32, i32, i32, i32)>> {
    let reply = hyprland_request("j/monitors")?;
    let value: serde_json::Value = serde_json::from_str(&reply).ok()?;
    let mut monitors = Vec::new();
    for monitor in value.as_array()? {
        let name = monitor.get("name")?.as_str()?.to_owned();
        let left = monitor.get("x")?.as_i64()? as i32;
        let top = monitor.get("y")?.as_i64()? as i32;
        let width = monitor.get("width")?.as_i64()? as i32;
        let height = monitor.get("height")?.as_i64()? as i32;
        let scale = monitor.get("scale").and_then(|v| v.as_f64()).unwrap_or(1.0);
        let scale = if scale > 0.0 { scale } else { 1.0 };
        monitors.push((
            name,
            left,
            top,
            (f64::from(width) / scale).round() as i32,
            (f64::from(height) / scale).round() as i32,
        ));
    }
    (!monitors.is_empty()).then_some(monitors)
}

/// X11 侧每个显示器的矩形，按键名对齐到 Hyprland 的逻辑矩形。
///
/// 两套坐标空间完全不同：XWayland 把显示器按物理像素横排（本机是
/// 4800x1800，eDP-1 在左、HDMI-A-3 在右），Hyprland 按逻辑像素竖排
/// （2304x2520，HDMI-A-3 在上、eDP-1 在下）。绝对定位要把程序算出的
/// X11 全局坐标换算到 Hyprland 空间。
fn screen_rect_maps() -> Vec<(String, (i32, i32, i32, i32), (i32, i32, i32, i32))> {
    let Ok(topology) = super::display::topology::Topology::query(true) else {
        return Vec::new();
    };
    let x11 = topology.monitors();
    let Some(hypr) = hyprland_monitors() else {
        return Vec::new();
    };
    let mut maps = Vec::new();
    for monitor in &x11 {
        let Some((_, hx, hy, hw, hh)) = hypr.iter().find(|(name, ..)| *name == monitor.name) else {
            continue;
        };
        maps.push((
            monitor.name.clone(),
            (
                monitor.left,
                monitor.top,
                monitor.width as i32,
                monitor.height as i32,
            ),
            (*hx, *hy, *hw, *hh),
        ));
    }
    maps
}

/// 通过 `zwp_virtual_keyboard_v1` 输入文本。
///
/// 与 uinput 不同，这条协议允许客户端**自带 xkb keymap**，因此可以输入键位表
/// 里不存在的字符（中文等）。Hyprland 暴露 `zwp_virtual_keyboard_manager_v1`。
///
/// 注意：XWayland 客户端使用的是 X 自己的键位表，虚拟键盘的自定义 keymap 对它
/// 无效，那类窗口仍走 XTest。
mod virtual_keyboard {
    use anyhow::{Context, Result};
    use std::os::fd::AsFd;
    use std::time::Duration;
    use wayland_client::globals::{GlobalList, registry_queue_init};
    use wayland_client::protocol::{wl_registry, wl_seat};
    use wayland_client::{Connection, Dispatch, EventQueue, QueueHandle};
    use wayland_protocols_misc::zwp_virtual_keyboard_v1::client::{
        zwp_virtual_keyboard_manager_v1 as manager, zwp_virtual_keyboard_v1 as keyboard,
    };

    struct State;

    impl Dispatch<wl_registry::WlRegistry, wayland_client::globals::GlobalListContents> for State {
        fn event(
            _: &mut Self,
            _: &wl_registry::WlRegistry,
            _: wl_registry::Event,
            _: &wayland_client::globals::GlobalListContents,
            _: &Connection,
            _: &QueueHandle<Self>,
        ) {
        }
    }
    wayland_client::delegate_noop!(State: ignore manager::ZwpVirtualKeyboardManagerV1);
    wayland_client::delegate_noop!(State: ignore keyboard::ZwpVirtualKeyboardV1);
    wayland_client::delegate_noop!(State: ignore wl_seat::WlSeat);

    /// 发送给合成器的第一个键码。协议里传的是 **evdev 风格**键码，合成器
    /// 与客户端都会 `+8` 得到 xkb keycode（Hyprland `Keyboard.cpp`：
    /// `updateXkbStateWithKey(event.key + 8, ...)`），所以 keymap 里声明的
    /// 键码必须是 `FIRST_CODE + KEYCODE_OFFSET`。与 wtype 一致取 1。
    const FIRST_CODE: u8 = 1;
    /// evdev 键码到 xkb keycode 的固定偏移。
    const KEYCODE_OFFSET: u8 = 8;

    pub(crate) struct Keyboard {
        /// Held so the queue and the globals registry stay connected.
        connection: Connection,
        globals: GlobalList,
        queue: EventQueue<State>,
    }

    /// 生成只包含所需字符的 keymap，返回 (字节, 覆盖的字符数)。
    fn keymap_for(text: &str) -> (String, usize) {
        let mut chars: Vec<char> = Vec::new();
        for c in text.chars() {
            if !chars.contains(&c) && chars.len() < 56 {
                chars.push(c);
            }
        }
        let mut out = String::from("xkb_keymap {\n  xkb_keycodes \"vk\" {\n");
        out.push_str(&format!("    minimum = {};\n", FIRST_CODE + KEYCODE_OFFSET));
        out.push_str(&format!(
            "    maximum = {};\n",
            FIRST_CODE as usize + KEYCODE_OFFSET as usize + chars.len().saturating_sub(1)
        ));
        for index in 0..chars.len() {
            out.push_str(&format!(
                "    <VK{index:02}> = {};\n",
                FIRST_CODE as usize + KEYCODE_OFFSET as usize + index
            ));
        }
        out.push_str("  };\n  xkb_types \"vk\" { include \"complete\" };\n");
        out.push_str("  xkb_compat \"vk\" { include \"complete\" };\n");
        out.push_str("  xkb_symbols \"vk\" {\n");
        for (index, c) in chars.iter().enumerate() {
            let point = *c as u32;
            let sym = if point < 0x100 {
                point
            } else {
                0x0100_0000 | point
            };
            out.push_str(&format!("    key <VK{index:02}> {{ [ 0x{sym:08x} ] }};\n"));
        }
        out.push_str("  };\n};\n");
        (out, chars.len())
    }

    fn keymap_fd(bytes: &[u8]) -> Result<std::os::fd::OwnedFd> {
        use std::os::fd::FromRawFd;
        let name = c"openuuyc-vk";
        let raw = unsafe { libc::memfd_create(name.as_ptr(), 0) };
        anyhow::ensure!(raw >= 0, "创建 keymap 内存文件失败");
        let mut file = unsafe { std::fs::File::from_raw_fd(raw) };
        use std::io::Write as _;
        file.write_all(bytes).context("写入 keymap 失败")?;
        Ok(std::os::fd::OwnedFd::from(file))
    }

    impl Keyboard {
        pub(crate) fn connect() -> Result<Self> {
            let connection = Connection::connect_to_env().context("连接 Wayland 失败")?;
            let (globals, queue): (GlobalList, EventQueue<State>) =
                registry_queue_init(&connection).context("枚举 Wayland 全局对象失败")?;
            // Bind once up front so a desktop without the protocol is reported
            // at startup rather than on the first keystroke.
            {
                let qh = queue.handle();
                let _manager: manager::ZwpVirtualKeyboardManagerV1 =
                    globals
                        .bind(&qh, 1..=1, ())
                        .context("桌面没有提供 zwp_virtual_keyboard_manager_v1")?;
                let _seat: wl_seat::WlSeat =
                    globals.bind(&qh, 1..=8, ()).context("没有 Wayland seat")?;
            }
            queue.flush().context("提交虚拟键盘请求失败")?;
            Ok(Self {
                connection,
                globals,
                queue,
            })
        }

        /// 输入一段文本：新建一个虚拟键盘对象，上传含这些字符的 keymap，
        /// 逐个按键，最后销毁该对象。
        ///
        /// **每次调用都必须新建对象。** Hyprland 只在对象第一次收到 keymap
        /// 时建立键位映射，之后在同一对象上重新上传 keymap 不会生效；手机输入
        /// 法逐字符发送（每次 `chars=1`），复用对象会让每个字符都按第一个字符
        /// 的键位输入（输入 `XYZ` 得到 `XXX`）。wtype 同样每次新建对象。
        pub(crate) fn text(&mut self, text: &str, permitted: &dyn Fn() -> bool) -> Result<()> {
            let (keymap, bound) = keymap_for(text);
            if bound == 0 {
                return Ok(());
            }
            let qh = self.queue.handle();
            let manager: manager::ZwpVirtualKeyboardManagerV1 =
                self.globals
                    .bind(&qh, 1..=1, ())
                    .context("桌面没有提供 zwp_virtual_keyboard_manager_v1")?;
            let seat: wl_seat::WlSeat = self
                .globals
                .bind(&qh, 1..=8, ())
                .context("没有 Wayland seat")?;
            let keyboard = manager.create_virtual_keyboard(&seat, &qh, ());
            // keymap 必须在上报按键之前上传，且要等合成器处理完
            // （否则合成器会报 NO_KEYMAP）。xkbcommon 的
            // xkb_keymap_new_from_string 要求 NUL 结尾，长度也要算上。
            let mut bytes = keymap.into_bytes();
            bytes.push(0);
            let fd = keymap_fd(&bytes)?;
            keyboard.keymap(1, fd.as_fd(), bytes.len() as u32);
            self.queue
                .roundtrip(&mut State)
                .context("等待虚拟键盘 keymap 生效失败")?;
            // 清掉可能残留的修饰键状态。
            keyboard.modifiers(0, 0, 0, 0);
            let _ = self.queue.roundtrip(&mut State);

            let mut index = 0u8;
            let mut seen: Vec<char> = Vec::new();
            for c in text.chars() {
                if !permitted() {
                    break;
                }
                let position = match seen.iter().position(|seen| *seen == c) {
                    Some(found) => found as u8,
                    None => {
                        if seen.len() >= 56 {
                            continue;
                        }
                        seen.push(c);
                        let assigned = index;
                        index += 1;
                        assigned
                    }
                };
                // 发送 evdev 风格键码；合成器与客户端会各自 +8 查 keymap。
                let code = u32::from(FIRST_CODE + position);
                keyboard.key(0, code, 1);
                let _ = self.queue.roundtrip(&mut State);
                keyboard.key(0, code, 0);
                let _ = self.queue.roundtrip(&mut State);
                // 输入法（IBus）会丢过快的按键。
                std::thread::sleep(Duration::from_millis(4));
            }
            keyboard.destroy();
            let _ = self.queue.roundtrip(&mut State);
            let _ = &self.connection;
            Ok(())
        }
    }
}

/// 绝对指针覆盖的区域：合成器全局布局的外接矩形，左上角加尺寸。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Desktop {
    pub left: i32,
    pub top: i32,
    pub width: u32,
    pub height: u32,
}

/// 合成器全局布局的外接矩形，包括没有名字的输出：合成器把绝对指针映射到
/// 它全部输出的外接矩形上（wlroots 的 output layout、Hyprland、niri、KWin 的
/// 工作区、Mutter 的 stage），少算一块屏落点就整体偏。
pub(crate) fn layout_bounds(layout: &crate::platform::CompositorLayout) -> Option<Desktop> {
    let mut outputs = layout
        .outputs
        .iter()
        .filter(|o| o.width > 0 && o.height > 0);
    let first = outputs.next()?;
    let span = |o: &crate::platform::CompositorOutput| {
        (
            i64::from(o.left),
            i64::from(o.top),
            i64::from(o.left) + i64::from(o.width),
            i64::from(o.top) + i64::from(o.height),
        )
    };
    let (mut left, mut top, mut right, mut bottom) = span(first);
    for output in outputs {
        let (l, t, r, b) = span(output);
        left = left.min(l);
        top = top.min(t);
        right = right.max(r);
        bottom = bottom.max(b);
    }
    Some(Desktop {
        left: i32::try_from(left).ok()?,
        top: i32::try_from(top).ok()?,
        width: u32::try_from(right - left).ok()?,
        height: u32::try_from(bottom - top).ok()?,
    })
}

/// 合成器把一个轴值落到像素 `origin + floor(value × extent / (AXIS_MAX + 1))`
/// 上（libinput 按 `max - min + 1` 换算）。取像素中心对应的值，取整之后正好
/// 落回这个像素。超过 `AXIS_MAX + 1` 像素宽的布局会有相邻像素共用一个值。
fn axis_value(position: i32, origin: i32, extent: u32) -> i32 {
    if extent == 0 {
        return 0;
    }
    let range = i64::from(uinput::AXIS_MAX) + 1;
    let extent = i64::from(extent);
    let offset = (i64::from(position) - i64::from(origin)).clamp(0, extent - 1);
    ((2 * offset + 1) * range / (2 * extent)).clamp(0, i64::from(uinput::AXIS_MAX)) as i32
}

/// 内核丢弃与设备当前值相同的绝对坐标，所以要重写同一个值时先在单独一帧里
/// 挪开一个单位（不到一像素）。在轴的最大值处往回挪，不能出界。只有两条轴
/// 都相同才需要：只丢一条轴时，合成器沿用设备自己的那个值，正是目标值。
fn nudge(last: (i32, i32), value: (i32, i32)) -> Option<(i32, i32)> {
    (last == value).then(|| {
        let x = if value.0 >= uinput::AXIS_MAX {
            value.0 - 1
        } else {
            value.0 + 1
        };
        (x, value.1)
    })
}

/// 合成器接手一台刚建的 uinput 设备要多久。这段时间里写进去的绝对坐标谁都
/// 收不到（evdev 只把 open 之后的事件交给读者），所以要反复补写。
const FRESH_FOR: Duration = Duration::from_millis(1500);

/// 上面那个窗口期里补写的间隔。
const REPEAT_EVERY: Duration = Duration::from_millis(250);

/// 窗口期里还要补写的位置。
struct Repeat {
    until: std::time::Instant,
    next: std::time::Instant,
    value: (i32, i32),
}

/// 由合成器解算的绝对指针。轴范围固定（与 Sunshine 一样），按布局外接矩形
/// 换算，所以布局变了也不用换设备。
struct Absolute {
    device: uinput::Device,
    made: std::time::Instant,
    /// 设备轴上的当前值（内核里的那份），新建时为 0。
    last: (i32, i32),
    repeat: Option<Repeat>,
}

impl Absolute {
    fn create() -> Result<Self> {
        Ok(Self {
            device: uinput::Device::open_absolute()?,
            made: std::time::Instant::now(),
            last: (0, 0),
            repeat: None,
        })
    }

    /// 写入一对轴值。与当前值相同时内核会把它整帧丢掉，而光标可能已经被别的
    /// 设备（相对设备、本机鼠标、程序自己移动指针）挪走了，所以先在单独一帧里
    /// 挪开一个单位，再写目标值。
    fn emit(&mut self, value: (i32, i32)) -> Result<()> {
        if let Some((x, y)) = nudge(self.last, value) {
            self.device.move_absolute(x, y)?;
        }
        self.device.move_absolute(value.0, value.1)?;
        self.last = value;
        Ok(())
    }

    /// 把指针在原地重报一次。`emit` 对同一个值会先在单独一帧里挪开一个单位再回
    /// 来：合成器看到的是两次真的移动，而指针最终停在原地。
    fn realign(&mut self) -> Result<()> {
        self.emit(self.last)
    }

    /// 把合成器空间里的一点写进设备，设备还新的时候安排补写。
    fn aim(&mut self, x: i32, y: i32, bounds: Desktop) -> Result<()> {
        let value = (
            axis_value(x, bounds.left, bounds.width),
            axis_value(y, bounds.top, bounds.height),
        );
        self.emit(value)?;
        let now = std::time::Instant::now();
        let until = self.made + FRESH_FOR;
        self.repeat = (now < until).then(|| Repeat {
            until,
            next: now + REPEAT_EVERY,
            value,
        });
        Ok(())
    }

    /// 窗口期里按间隔补写。补写的值与当前值相同，`emit` 会先挪开一个单位，
    /// 每次补写都以目标值结束，窗口结束时不必再写。
    fn flush(&mut self) -> Result<()> {
        let Some(repeat) = &mut self.repeat else {
            return Ok(());
        };
        let now = std::time::Instant::now();
        if now >= repeat.until {
            self.repeat = None;
            return Ok(());
        }
        if now < repeat.next {
            return Ok(());
        }
        repeat.next = now + REPEAT_EVERY;
        let value = repeat.value;
        self.emit(value)
    }
}

/// 「请指针在原地动一下」的入口。
///
/// 合成器只在指针真的动过之后才重新判定指针底下是谁：niri 的 `layout.dnd_update`
/// 只在 `on_pointer_motion*` 里调用，没有任何 surface 上屏时的重判。而 Wayland 拖放
/// 的接取面正是摆在指针底下的（见 `drag_drop::wayland`），所以摆好之后要有人推指针
/// 一把，合成器才会把那次拖放交过来。
///
/// 那条路在 `features/host/input` 之外，拿不到引擎句柄，所以引擎启动时在这里登记一
/// 条只有「动一下」的通道——走的就是它平时那条绝对定位，不新建任何注入设备。
static REALIGN: std::sync::Mutex<Option<std::sync::mpsc::SyncSender<()>>> =
    std::sync::Mutex::new(None);

/// 引擎启动时登记，拿到的接收端由引擎自己在循环里清。
pub(crate) fn realign_channel() -> std::sync::mpsc::Receiver<()> {
    let (tx, rx) = std::sync::mpsc::sync_channel(4);
    *REALIGN.lock().unwrap_or_else(|error| error.into_inner()) = Some(tx);
    rx
}

/// 请指针在原地动一下。引擎没起来、或者这一下没人接（通道满了）时返回 false。
pub(crate) fn realign_pointer() -> bool {
    REALIGN
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .as_ref()
        .is_some_and(|tx| tx.try_send(()).is_ok())
}

pub(crate) struct Injector {
    connection: RustConnection,
    root: Window,
    min_keycode: Keycode,
    max_keycode: Keycode,
    /// 可用时优先进到这里，X11 只负责读键位表与指针位置。
    uinput: Option<uinput::Device>,
    /// 由合成器按它自己的全局布局解算的绝对指针。问得到合成器布局时才有，
    /// 没有就退回下面这条相对路径。
    absolute: Option<Absolute>,
    /// 多点触摸屏，拿不到就没有：触摸退回单指模拟。
    touchscreen: Option<uinput::Touchscreen>,
    /// X11 矩形 → Hyprland 逻辑矩形的映射（按显示器名对齐）。
    screen_maps: Vec<(String, (i32, i32, i32, i32), (i32, i32, i32, i32))>,
    /// 原生 Wayland 客户端的文字输入（可携带自定义 keymap）。
    virtual_keyboard: Option<virtual_keyboard::Keyboard>,
}

impl Injector {
    pub fn open() -> Result<Self> {
        let (connection, screen) = x11rb::connect(None).context("连接 X11 显示失败")?;
        ensure!(
            connection
                .extension_information(x11rb::protocol::xtest::X11_EXTENSION_NAME)?
                .is_some(),
            "X 服务器没有 XTEST 扩展，无法注入输入"
        );
        connection.xtest_get_version(2, 2)?.reply()?;
        let setup = connection.setup();
        let root = setup.roots[screen].root;
        let (min_keycode, max_keycode) = (setup.min_keycode, setup.max_keycode);
        // Xorg delivers XTest to every client and positions absolutely;
        // uinput there would only add pointer acceleration.
        let wayland = super::capture::wayland_session();
        let uinput = if !wayland {
            None
        } else {
            match uinput::Device::open() {
                Ok(device) => {
                    tracing::info!("启用 uinput 输入注入（键盘、按键与相对位移）");
                    Some(device)
                }
                Err(error) => {
                    tracing::warn!(%error, "无法使用 uinput，回退到 XTest（只影响 XWayland 客户端）");
                    None
                }
            }
        };
        // 绝对定位另建一台设备，越早建越好：合成器接手它要几百毫秒。轴范围
        // 固定，用不用它由引擎按合成器布局决定。XTest 由 X 自己解算，用不着。
        let absolute = if uinput.is_some() {
            match Absolute::create() {
                Ok(absolute) => {
                    tracing::info!("启用 uinput 虚拟绝对指针（按合成器布局定位）");
                    Some(absolute)
                }
                Err(error) => {
                    tracing::warn!(%error, "虚拟绝对指针不可用，绝对定位改用相对移动");
                    None
                }
            }
        } else {
            None
        };
        // 触摸屏没有别的后备：没有它就退回用指针模拟单指，见 engine 里的 touch。
        // 一块从来收不到触点的触摸屏什么也不影响，所以它和设备一起常驻。
        let touchscreen = if uinput.is_some() {
            match uinput::Touchscreen::create() {
                Ok(touchscreen) => {
                    tracing::info!("虚拟触摸屏已就绪，控制器用平板触摸模式时由合成器派发多点触摸");
                    Some(touchscreen)
                }
                Err(error) => {
                    tracing::warn!(%error, "没有虚拟触摸屏，触摸退回单指模拟");
                    None
                }
            }
        } else {
            None
        };
        let screen_maps = if uinput.is_some() && hyprland() {
            let maps = screen_rect_maps();
            if maps.is_empty() {
                tracing::warn!("未能对齐 X11 与 Hyprland 的显示器布局，绝对定位可能偏移");
            } else {
                tracing::info!(monitors = maps.len(), "已对齐 X11 与 Hyprland 显示器布局");
            }
            maps
        } else {
            Vec::new()
        };
        let virtual_keyboard = if wayland {
            match virtual_keyboard::Keyboard::connect() {
                Ok(keyboard) => {
                    tracing::info!("启用 zwp_virtual_keyboard 文字输入（支持任意 Unicode）");
                    Some(keyboard)
                }
                Err(error) => {
                    tracing::warn!(%error, "虚拟键盘不可用，文字输入将回退 XTest");
                    None
                }
            }
        } else {
            None
        };
        Ok(Self {
            connection,
            root,
            min_keycode,
            max_keycode,
            uinput,
            absolute,
            touchscreen,
            screen_maps,
            virtual_keyboard,
        })
    }

    /// 有没有虚拟绝对指针。有它，引擎才可能按合成器布局定位。
    pub fn has_absolute_pointer(&self) -> bool {
        self.absolute.is_some()
    }

    /// 有没有虚拟触摸屏。Xorg 会话不建它，Wayland 下没有 `/dev/uinput` 写权限
    /// 或者建失败了也没有；写入失败后会被丢掉，之后也是没有。
    pub fn has_touchscreen(&self) -> bool {
        self.touchscreen.is_some()
    }

    /// 真实的多点触摸，由合成器派发给应用。返回 false 表示触摸屏已经没有了（见
    /// `has_touchscreen`），调用方要退回单指模拟。
    pub fn touch(&mut self, kind: u8, points: &[Touch], rotation: Rotation) -> Result<bool> {
        let Some(touchscreen) = &mut self.touchscreen else {
            return Ok(false);
        };
        let frames = touchscreen.step(kind, points, rotation)?;
        let written = touchscreen.write(&frames);
        self.keep_touchscreen(written)?;
        Ok(true)
    }

    /// 抬起所有触点。会话结束、换屏、取消时都要走一遍：合成器那边一个没抬起来
    /// 的触点会一直按着，应用也收不到结束。
    pub fn release_touch(&mut self) -> Result<()> {
        let Some(touchscreen) = &mut self.touchscreen else {
            return Ok(());
        };
        let released = touchscreen.release();
        self.keep_touchscreen(released)
    }

    /// 触摸屏写入失败就不再用它，和绝对指针一样：写到一半时合成器那边可能还按着
    /// 触点，而这边已经说不清是哪些了。丢掉设备，内核和 libinput 会在它移除时
    /// 把上面的触点全部抬起；之后的触摸退回单指模拟，日志也只记这一次。
    fn keep_touchscreen(&mut self, result: Result<()>) -> Result<()> {
        if let Err(error) = &result {
            tracing::warn!(%error, "虚拟触摸屏写入失败，停用它，触摸退回单指模拟");
            self.touchscreen = None;
        }
        result
    }

    /// 请合成器重新判定一次指针底下是谁：把绝对指针最后写下的位置重报一次。
    /// 返回「真的报了没有」：没有绝对指针（X11 会话、或者合成器认不出这台设备）
    /// 时这条路催不动它。指针最后一次不是经 [`Self::aim`] 摆的时候，那个位置已经
    /// 过期，调用方（引擎）不该再来请。
    pub fn realign(&mut self) -> Result<bool> {
        match self.absolute.as_mut() {
            Some(absolute) => absolute.realign().map(|()| true),
            None => Ok(false),
        }
    }

    /// 移到合成器全局空间里的一点，由合成器按 `bounds`（它全部输出的外接
    /// 矩形）解算。设备写入失败就不再用它，之后的绝对定位回到相对移动。
    pub fn aim(&mut self, x: i32, y: i32, bounds: Desktop) -> Result<()> {
        let absolute = self.absolute.as_mut().context("虚拟绝对指针不可用")?;
        let result = absolute.aim(x, y, bounds);
        if let Err(error) = &result {
            tracing::warn!(%error, "虚拟绝对指针写入失败，停用并改用相对移动");
            self.absolute = None;
        }
        result
    }

    /// 放弃还没补写完的位置（输入被收回或释放时，不再替对端移动指针）。
    pub fn forget_aim(&mut self) {
        if let Some(absolute) = &mut self.absolute {
            absolute.repeat = None;
        }
    }

    /// 当前焦点窗口是否跑在 XWayland 里。
    fn foreground_is_xwayland(&self) -> bool {
        let Some(reply) = hyprland_request("j/activewindow") else {
            return false;
        };
        serde_json::from_str::<serde_json::Value>(&reply)
            .ok()
            .and_then(|value| value.get("xwayland").and_then(|v| v.as_bool()))
            .unwrap_or(false)
    }

    /// 把程序给出的 X11 全局坐标换算到 Hyprland 的逻辑布局坐标。
    fn to_hyprland(&self, x: i32, y: i32) -> Option<(i32, i32)> {
        for (_, x11, hypr) in &self.screen_maps {
            let (left, top, width, height) = *x11;
            let (hx, hy, hw, hh) = *hypr;
            let (right, bottom) = (left + width, top + height);
            if width <= 0 || height <= 0 || x < left || x >= right || y < top || y >= bottom {
                continue;
            }
            let u = f64::from(x - left) / f64::from(width);
            let v = f64::from(y - top) / f64::from(height);
            return Some((
                hx + (f64::from(hw) * u).round() as i32,
                hy + (f64::from(hh) * v).round() as i32,
            ));
        }
        None
    }

    fn fake(&self, kind: u8, detail: u8, x: i16, y: i16) -> Result<()> {
        self.connection
            .xtest_fake_input(kind, detail, x11rb::CURRENT_TIME, self.root, x, y, 0)?
            .check()
            .context("注入输入事件失败")?;
        Ok(())
    }

    /// A key by Windows virtual-key code.
    pub fn key(&self, key: u16, down: bool) -> Result<()> {
        let code = evdev(key).with_context(|| format!("不支持的按键 {key:#04x}"))?;
        if let Some(device) = &self.uinput {
            return device.key(code, down);
        }
        let keycode = u8::try_from(code + 8).context("按键超出 X11 键码范围")?;
        ensure!(
            (self.min_keycode..=self.max_keycode).contains(&keycode),
            "按键超出 X 服务器键码范围"
        );
        self.fake(if down { KEY_PRESS } else { KEY_RELEASE }, keycode, 0, 0)
    }

    /// A button by X button number (1 left, 2 middle, 3 right, 8/9 side).
    pub fn button(&self, button: u8, down: bool) -> Result<()> {
        if let Some(device) = &self.uinput {
            return device.button(button, down);
        }
        self.fake(
            if down { BUTTON_PRESS } else { BUTTON_RELEASE },
            button,
            0,
            0,
        )
    }

    /// Move to a root-window position. Positions in the compositor's own
    /// layout go through [`Self::aim`] instead.
    pub fn move_to(&self, x: i32, y: i32) -> Result<()> {
        if let Some(device) = &self.uinput {
            // 设备不声明 ABS 轴（否则会被 libinput 误分类），所以按当前
            // 指针位置换算成相对位移。位置以合成器为准：每次重新读取，
            // 所以加速造成的单步偏差不会累积。
            let (cx, cy) = hyprland_cursor()
                .or_else(|| self.pointer().ok())
                .context("无法获取当前指针位置")?;
            let (tx, ty) = self.to_hyprland(x, y).unwrap_or((x, y));
            let (dx, dy) = (tx - cx, ty - cy);
            if dx == 0 && dy == 0 {
                return Ok(());
            }
            return device.move_by(dx, dy);
        }
        self.fake(
            MOTION_NOTIFY,
            0,
            i16::try_from(x).context("指针坐标超出 X11 范围")?,
            i16::try_from(y).context("指针坐标超出 X11 范围")?,
        )
    }

    /// Move by a pixel delta from wherever the pointer is.
    pub fn move_by(&self, mut dx: i32, mut dy: i32) -> Result<()> {
        if let Some(device) = &self.uinput {
            return device.move_by(dx, dy);
        }
        while dx != 0 || dy != 0 {
            let x = dx.clamp(i16::MIN.into(), i16::MAX.into());
            let y = dy.clamp(i16::MIN.into(), i16::MAX.into());
            self.fake(MOTION_NOTIFY, 1, x as i16, y as i16)?;
            dx -= x;
            dy -= y;
        }
        Ok(())
    }

    /// One wheel notch: X delivers wheels as buttons 4-7.
    pub fn wheel_notch(&self, horizontal: bool, positive: bool) -> Result<()> {
        if let Some(device) = &self.uinput {
            return device.wheel(horizontal, positive);
        }
        // Windows: positive vertical is away from the user (up), positive
        // horizontal is to the right.
        let button = match (horizontal, positive) {
            (false, true) => 4,
            (false, false) => 5,
            (true, false) => 6,
            (true, true) => 7,
        };
        self.button(button, true)?;
        self.button(button, false)
    }

    pub fn toggled(&self, lock: Lock) -> Result<bool> {
        let leds = self.connection.get_keyboard_control()?.reply()?.led_mask;
        // Core LEDs 1-3 are Caps, Num and Scroll Lock on every X keyboard map.
        Ok(leds
            & match lock {
                Lock::Caps => 1,
                Lock::Num => 2,
                Lock::Scroll => 4,
            }
            != 0)
    }

    pub fn pointer(&self) -> Result<(i32, i32)> {
        let reply = self.connection.query_pointer(self.root)?.reply()?;
        Ok((reply.root_x.into(), reply.root_y.into()))
    }

    /// Type text independently of the key layout, the way Windows' Unicode
    /// keyboard input does: each character's keysym is found in the current
    /// map, or bound for the moment to a spare keycode.
    ///
    /// Unicode input on Windows ignores the keys being held, so modifiers the
    /// controller still holds (the Ctrl of the Ctrl+V that asked for a paste)
    /// are released while typing and pressed again afterwards, and Caps Lock
    /// does not change the case of a letter. A Windows line break (CR LF)
    /// is one Enter.
    pub fn text(&mut self, text: &str, permitted: impl Fn() -> bool) -> Result<()> {
        // 原生 Wayland 客户端走虚拟键盘（XTest 到不了它们）；
        // XWayland 客户端用 X 自己的键位表，自带 keymap 的虚拟键盘对它无效，
        // 因此仍然走 XTest。焦点不在 Hyprland 的窗口上时按原生处理。
        let xwayland = self.foreground_is_xwayland();
        tracing::info!(
            chars = text.chars().count(),
            xwayland,
            virtual_keyboard = self.virtual_keyboard.is_some(),
            "文字输入"
        );
        if let Some(keyboard) = &mut self.virtual_keyboard
            && !xwayland
        {
            match keyboard.text(text, &permitted) {
                Ok(()) => {
                    tracing::info!("文字输入：虚拟键盘路径完成");
                    return Ok(());
                }
                Err(error) => {
                    tracing::warn!(%error, "虚拟键盘输入失败，回退 XTest");
                }
            }
        } else {
            tracing::info!("文字输入：使用 XTest 路径");
        }
        let count = self.max_keycode - self.min_keycode + 1;
        let map = self
            .connection
            .get_keyboard_mapping(self.min_keycode, count)?
            .reply()?;
        let per = usize::from(map.keysyms_per_keycode);
        ensure!(per > 0, "键盘映射无效");
        let find = |sym: Keysym| -> Option<(Keycode, bool)> {
            map.keysyms.chunks(per).enumerate().find_map(|(i, syms)| {
                let code = self.min_keycode + i as u8;
                if syms.first() == Some(&sym) {
                    Some((code, false))
                } else if syms.get(1) == Some(&sym) {
                    Some((code, true))
                } else {
                    None
                }
            })
        };
        let spare = map
            .keysyms
            .chunks(per)
            .rposition(|syms| syms.iter().all(|s| *s == NO_SYMBOL))
            .map(|i| self.min_keycode + i as u8);
        let shift = evdev(0xa0)
            .map(|code| (code + 8) as u8)
            .context("缺少 Shift 键")?;
        let caps = self.toggled(Lock::Caps).unwrap_or(false);
        let held = self.held_modifiers()?;
        for code in &held {
            self.fake(KEY_RELEASE, *code, 0, 0)?;
        }
        // Caps Lock changes the case of letters, including those bound to the
        // spare keycode, so it is off while typing.
        if caps {
            self.tap(0x14)?;
        }
        let mut bound = false;
        let result = (|| -> Result<()> {
            let mut previous = None;
            for c in text.chars() {
                ensure!(permitted(), "文字输入已取消");
                let after_cr = previous == Some('\r');
                previous = Some(c);
                if c == '\n' && after_cr {
                    continue;
                }
                let sym = keysym(c);
                let (code, shifted) = match find(sym) {
                    Some(found) => found,
                    None => {
                        let spare = spare.context("没有空闲键码可用于输入该字符")?;
                        let syms = vec![sym; per];
                        self.connection
                            .change_keyboard_mapping(1, spare, per as u8, &syms)?
                            .check()?;
                        bound = true;
                        // Clients refresh their keymap on MappingNotify; give
                        // the focused one a moment before the key arrives.
                        self.connection.get_input_focus()?.reply()?;
                        std::thread::sleep(Duration::from_millis(12));
                        (spare, false)
                    }
                };
                if shifted {
                    self.fake(KEY_PRESS, shift, 0, 0)?;
                }
                let typed = self
                    .fake(KEY_PRESS, code, 0, 0)
                    .and_then(|()| self.fake(KEY_RELEASE, code, 0, 0));
                if shifted {
                    self.fake(KEY_RELEASE, shift, 0, 0)?;
                }
                typed?;
                // Input methods (IBus) pass each key through another process
                // and drop keys that arrive faster than they are handled.
                std::thread::sleep(if bound {
                    Duration::from_millis(12)
                } else {
                    TEXT_PACE
                });
            }
            Ok(())
        })();
        if bound && let Some(spare) = spare {
            let empty = vec![NO_SYMBOL; per];
            let _ = self
                .connection
                .change_keyboard_mapping(1, spare, per as u8, &empty);
            let _ = self.connection.flush();
        }
        if caps {
            let _ = self.tap(0x14);
        }
        for code in &held {
            let _ = self.fake(KEY_PRESS, *code, 0, 0);
        }
        result
    }

    /// Press and release a key by Windows virtual-key code.
    fn tap(&self, key: u16) -> Result<()> {
        self.key(key, true)?;
        self.key(key, false)
    }

    /// Modifier keys other than the lock keys that are down right now.
    fn held_modifiers(&self) -> Result<Vec<Keycode>> {
        let mapping = self.connection.get_modifier_mapping()?.reply()?;
        let keys = self.connection.query_keymap()?.reply()?.keys;
        let per = usize::from(mapping.keycodes_per_modifier()).max(1);
        let mut held = Vec::new();
        for (index, codes) in mapping.keycodes.chunks(per).enumerate() {
            // Lock (index 1) is a toggle: pressing it again would flip it.
            if index == 1 {
                continue;
            }
            for &code in codes {
                if code != 0
                    && keys[usize::from(code / 8)] & (1 << (code % 8)) != 0
                    && !held.contains(&code)
                {
                    held.push(code);
                }
            }
        }
        Ok(held)
    }

    /// The process that owns the active window, as the window manager reports.
    pub fn foreground(&self) -> Option<u32> {
        let atom = |name: &str| {
            self.connection
                .intern_atom(true, name.as_bytes())
                .ok()?
                .reply()
                .ok()
                .map(|r| r.atom)
                .filter(|atom| *atom != 0)
        };
        let active = atom("_NET_ACTIVE_WINDOW")?;
        let pid = atom("_NET_WM_PID")?;
        let window = self
            .connection
            .get_property(false, self.root, active, AtomEnum::WINDOW, 0, 1)
            .ok()?
            .reply()
            .ok()?
            .value32()?
            .next()?;
        self.connection
            .get_property(false, window, pid, AtomEnum::CARDINAL, 0, 1)
            .ok()?
            .reply()
            .ok()?
            .value32()?
            .next()
    }

    pub fn flush(&mut self) -> Result<()> {
        // 合成器刚接手的设备上，位置可能要写上好几遍才有人收到。
        if let Some(absolute) = &mut self.absolute
            && let Err(error) = absolute.flush()
        {
            tracing::warn!(%error, "虚拟绝对指针写入失败，停用并改用相对移动");
            self.absolute = None;
            return Err(error);
        }
        self.connection.flush()?;
        Ok(())
    }
}

/// Lock the session through logind, as Win+L does on Windows.
///
/// A program the desktop starts runs in the user manager, outside any logind
/// session, so `loginctl lock-session` without a name cannot find one. `auto`
/// falls back to the user's graphical session.
pub(crate) fn lock_session() -> Result<()> {
    let status = std::process::Command::new("loginctl")
        .args(["lock-session", "auto"])
        .status()
        .context("无法调用 loginctl 锁定会话")?;
    ensure!(status.success(), "锁定会话失败：{status}");
    Ok(())
}

/// Open the desktop's system monitor, the counterpart of Task Manager.
pub(crate) fn system_monitor() -> Result<()> {
    for program in [
        "gnome-system-monitor",
        "plasma-systemmonitor",
        "ksysguard",
        "mate-system-monitor",
        "xfce4-taskmanager",
    ] {
        match std::process::Command::new(program)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
        {
            Ok(mut child) => {
                std::thread::spawn(move || child.wait());
                return Ok(());
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error.into()),
        }
    }
    bail!("没有找到系统监视器程序")
}

fn keysym(c: char) -> Keysym {
    match c {
        '\n' | '\r' => 0xff0d,
        '\t' => 0xff09,
        '\u{8}' => 0xff08,
        // Latin-1 keysyms equal their code points; everything else uses the
        // Unicode keysym range.
        ' '..='~' | '\u{a0}'..='\u{ff}' => c as u32,
        _ => 0x0100_0000 | c as u32,
    }
}

/// Windows virtual-key code to evdev key code, by physical position (US
/// names). The same table as the viewer's winit mapping, read backwards.
pub(crate) fn evdev(key: u16) -> Option<u16> {
    Some(match key {
        0x08 => 14,  // Backspace
        0x09 => 15,  // Tab
        0x0d => 28,  // Enter
        0x10 => 42,  // Shift
        0x11 => 29,  // Control
        0x12 => 56,  // Alt
        0x13 => 119, // Pause
        0x14 => 58,  // Caps Lock
        0x15 => 122, // Kana / Hangul
        0x19 => 123, // Hanja
        0x1b => 1,   // Escape
        0x1c => 92,  // Convert (Henkan)
        0x1d => 94,  // NonConvert (Muhenkan)
        0x20 => 57,  // Space
        0x21 => 104, // Page Up
        0x22 => 109, // Page Down
        0x23 => 107, // End
        0x24 => 102, // Home
        0x25 => 105, // Left
        0x26 => 103, // Up
        0x27 => 106, // Right
        0x28 => 108, // Down
        0x2c => 99,  // Print Screen
        0x2d => 110, // Insert
        0x2e => 111, // Delete
        0x2f => 138, // Help
        0x30 => 11,
        0x31..=0x39 => key - 0x31 + 2,
        0x41 => 30,
        0x42 => 48,
        0x43 => 46,
        0x44 => 32,
        0x45 => 18,
        0x46 => 33,
        0x47 => 34,
        0x48 => 35,
        0x49 => 23,
        0x4a => 36,
        0x4b => 37,
        0x4c => 38,
        0x4d => 50,
        0x4e => 49,
        0x4f => 24,
        0x50 => 25,
        0x51 => 16,
        0x52 => 19,
        0x53 => 31,
        0x54 => 20,
        0x55 => 22,
        0x56 => 47,
        0x57 => 17,
        0x58 => 45,
        0x59 => 21,
        0x5a => 44,
        0x5b => 125, // Left Windows / Super
        0x5c => 126, // Right Windows / Super
        0x5d => 127, // Menu
        0x5f => 142, // Sleep
        0x60 => 82,  // Numpad 0
        0x61 => 79,
        0x62 => 80,
        0x63 => 81,
        0x64 => 75,
        0x65 => 76,
        0x66 => 77,
        0x67 => 71,
        0x68 => 72,
        0x69 => 73,                      // Numpad 9
        0x6a => 55,                      // Numpad *
        0x6b => 78,                      // Numpad +
        0x6c => 121,                     // Numpad separator
        0x6d => 74,                      // Numpad -
        0x6e => 83,                      // Numpad .
        0x6f => 98,                      // Numpad /
        0x70..=0x79 => key - 0x70 + 59,  // F1-F10
        0x7a => 87,                      // F11
        0x7b => 88,                      // F12
        0x7c..=0x87 => key - 0x7c + 183, // F13-F24
        0x90 => 69,                      // Num Lock
        0x91 => 70,                      // Scroll Lock
        0xa0 => 42,                      // Left Shift
        0xa1 => 54,                      // Right Shift
        0xa2 => 29,                      // Left Control
        0xa3 => 97,                      // Right Control
        0xa4 => 56,                      // Left Alt
        0xa5 => 100,                     // Right Alt
        0xa6 => 158,                     // Browser Back
        0xa7 => 159,                     // Browser Forward
        0xa8 => 173,                     // Browser Refresh
        0xa9 => 128,                     // Browser Stop
        0xaa => 217,                     // Browser Search
        0xab => 156,                     // Browser Favorites
        0xac => 172,                     // Browser Home
        0xad => 113,                     // Volume Mute
        0xae => 114,                     // Volume Down
        0xaf => 115,                     // Volume Up
        0xb0 => 163,                     // Next Track
        0xb1 => 165,                     // Previous Track
        0xb2 => 166,                     // Stop
        0xb3 => 164,                     // Play/Pause
        0xb4 => 155,                     // Mail
        0xb5 => 226,                     // Media Select
        0xb6 => 157,                     // Launch App 1 (Computer)
        0xb7 => 140,                     // Launch App 2 (Calculator)
        0xba => 39,                      // ;
        0xbb => 13,                      // =
        0xbc => 51,                      // ,
        0xbd => 12,                      // -
        0xbe => 52,                      // .
        0xbf => 53,                      // /
        0xc0 => 41,                      // `
        0xc1 => 89,                      // Ro
        0xc2 => 121,                     // ABNT C2 / Numpad ,
        0xdb => 26,                      // [
        0xdc => 43,                      // \
        0xdd => 27,                      // ]
        0xde => 40,                      // '
        0xe2 => 86,                      // 102nd key
        0xff => 124,                     // Yen
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::evdev;

    #[test]
    fn letters_and_digits_follow_the_us_rows() {
        assert_eq!(evdev(b'Q'.into()), Some(16));
        assert_eq!(evdev(b'A'.into()), Some(30));
        assert_eq!(evdev(b'Z'.into()), Some(44));
        assert_eq!(evdev(b'1'.into()), Some(2));
        assert_eq!(evdev(b'9'.into()), Some(10));
        assert_eq!(evdev(0x70), Some(59));
        assert_eq!(evdev(0x79), Some(68));
        assert_eq!(evdev(0x7c), Some(183));
    }
}

#[cfg(test)]
mod absolute_tests {
    use super::{Desktop, axis_value, layout_bounds, nudge, uinput::AXIS_MAX};
    use crate::platform::{CompositorLayout, CompositorOutput, Rotation};

    /// What libinput hands the compositor (`value × extent / (max − min + 1)`)
    /// and the pixel that position falls on.
    fn landed(value: i32, origin: i32, extent: u32) -> i32 {
        let position = f64::from(value) * f64::from(extent) / (f64::from(AXIS_MAX) + 1.0);
        origin + position.floor() as i32
    }

    #[test]
    fn every_pixel_maps_back_onto_itself() {
        for (origin, extent) in [(0, 1), (0, 1000), (-1920, 4793), (1000, 3793), (0, 65536)] {
            for position in origin..origin + extent as i32 {
                let value = axis_value(position, origin, extent);
                assert!((0..=AXIS_MAX).contains(&value));
                assert_eq!(landed(value, origin, extent), position, "{origin} {extent}");
            }
        }
    }

    #[test]
    fn positions_outside_the_bounds_stay_on_its_edge() {
        assert_eq!(
            axis_value(-5000, -1920, 4793),
            axis_value(-1920, -1920, 4793)
        );
        assert_eq!(axis_value(9000, -1920, 4793), axis_value(2872, -1920, 4793));
        assert_eq!(axis_value(10, 0, 0), 0);
    }

    #[test]
    fn a_repeated_value_is_nudged_within_the_axis() {
        assert_eq!(nudge((10, 20), (11, 20)), None);
        assert_eq!(nudge((10, 20), (10, 21)), None);
        assert_eq!(nudge((10, 20), (10, 20)), Some((11, 20)));
        assert_eq!(nudge((0, 0), (0, 0)), Some((1, 0)));
        assert_eq!(nudge((AXIS_MAX, 5), (AXIS_MAX, 5)), Some((AXIS_MAX - 1, 5)));
    }

    #[test]
    fn bounds_cover_every_output_named_or_not() {
        let output = |name: Option<&str>, left, top, width, height| CompositorOutput {
            name: name.map(Into::into),
            left,
            top,
            width,
            height,
            rotation: Rotation::default(),
        };
        let layout = CompositorLayout {
            outputs: vec![
                output(Some("DP-1"), 0, 0, 2560, 1440),
                output(None, -1080, -480, 1080, 1920),
                output(Some("HDMI-A-1"), 2560, 200, 1280, 1024),
                output(Some("broken"), 9000, 9000, 0, 0),
            ],
        };
        assert_eq!(
            layout_bounds(&layout),
            Some(Desktop {
                left: -1080,
                top: -480,
                width: 4920,
                height: 1920
            })
        );
        assert_eq!(layout_bounds(&CompositorLayout::default()), None);
    }
}

#[cfg(test)]
mod session_tests {
    /// Needs a running Xorg session: `DISPLAY=:0 cargo test -- --ignored`.
    /// Moves the pointer and puts it back.
    #[test]
    #[ignore]
    fn pointer_moves_where_asked() {
        let mut injector = super::Injector::open().unwrap();
        let before = injector.pointer().unwrap();
        injector.move_to(123, 234).unwrap();
        injector.flush().unwrap();
        assert_eq!(injector.pointer().unwrap(), (123, 234));
        injector.move_by(10, -4).unwrap();
        assert_eq!(injector.pointer().unwrap(), (133, 230));
        println!(
            "caps lock on: {}",
            injector.toggled(super::Lock::Caps).unwrap()
        );
        println!("foreground pid: {:?}", injector.foreground());
        injector.move_to(before.0, before.1).unwrap();
        injector.flush().unwrap();
    }

    /// Types OPENUUYC_TYPE_TEXT into whatever has focus on DISPLAY.
    #[test]
    #[ignore = "types into the focused window of an X display"]
    fn types_text() {
        let text = std::env::var("OPENUUYC_TYPE_TEXT").unwrap();
        super::Injector::open()
            .unwrap()
            .text(&text, || true)
            .unwrap();
    }

    /// 需要真实会话和可写的 `/dev/uinput`：`cargo test -- --ignored`。
    ///
    /// 在屏幕右下角按下再抬起一次，验证这块触摸屏真能建起来、内核收下这些帧。
    /// 触点会落到那个位置的窗口上，所以跑的时候右下角最好什么都没有。
    #[test]
    #[ignore = "injects a real touch into the live session"]
    fn a_touchscreen_takes_a_contact_and_lets_it_go() {
        let mut injector = super::Injector::open().unwrap();
        let point = super::Touch {
            id: 1,
            x: 0.999,
            y: 0.999,
            radius: 0.01,
        };
        let rotation = crate::platform::Rotation::Normal;
        if !injector.touch(2, &[point], rotation).unwrap() {
            println!("这个会话没有触摸屏设备");
            return;
        }
        injector.release_touch().unwrap();
        injector.flush().unwrap();
    }
}
