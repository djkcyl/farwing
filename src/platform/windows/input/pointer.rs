//! Synthetic touch and pen injection (`CreateSyntheticPointerDevice` +
//! `InjectSyntheticPointerInput`, Windows 10 1809+;
//! `docs/protocol/touch-pen-brief.md` §2). The three functions are looked up
//! at run time: linking them would keep older systems from starting.
use anyhow::{Result, bail, ensure};
use std::sync::LazyLock;
use windows::Win32::{
    Foundation::{ERROR_NOT_READY, GetLastError, HANDLE, POINT, RECT},
    System::LibraryLoader::{GetModuleHandleW, GetProcAddress, LoadLibraryW},
    UI::{
        Input::Pointer::*,
        WindowsAndMessaging::{
            PEN_FLAG_BARREL, PEN_FLAG_ERASER, PEN_FLAG_INVERTED, PEN_MASK_PRESSURE, PEN_MASK_ROTATION,
            PEN_MASK_TILT_X, PEN_MASK_TILT_Y, POINTER_INPUT_TYPE, PT_PEN, PT_TOUCH, TOUCH_MASK_CONTACTAREA,
            TOUCH_MASK_ORIENTATION, TOUCH_MASK_PRESSURE,
        },
    },
};
use windows::core::{BOOL, w};

/// `POINTER_FEEDBACK_DEFAULT`: follow the host user's touch feedback setting.
const FEEDBACK_DEFAULT: i32 = 1;
/// Windows synthetic touch devices are created for this many contacts.
pub(crate) const TOUCH_SLOTS: u32 = 10;

#[repr(C)]
#[derive(Clone, Copy)]
union PointerUnion {
    touch: POINTER_TOUCH_INFO,
    pen: POINTER_PEN_INFO,
}

/// `POINTER_TYPE_INFO` (defined here so `Win32_UI_Controls` stays off).
#[repr(C)]
#[derive(Clone, Copy)]
struct PointerTypeInfo {
    kind: POINTER_INPUT_TYPE,
    info: PointerUnion,
}

type Create = unsafe extern "system" fn(POINTER_INPUT_TYPE, u32, i32) -> isize;
type Inject = unsafe extern "system" fn(isize, *const PointerTypeInfo, u32) -> BOOL;
type Destroy = unsafe extern "system" fn(isize);

struct Api {
    create: Create,
    inject: Inject,
    destroy: Destroy,
}

static API: LazyLock<Option<Api>> = LazyLock::new(|| unsafe {
    let user32 = GetModuleHandleW(w!("user32.dll"))
        .or_else(|_| LoadLibraryW(w!("user32.dll")))
        .ok()?;
    let create = GetProcAddress(user32, windows::core::s!("CreateSyntheticPointerDevice"))?;
    let inject = GetProcAddress(user32, windows::core::s!("InjectSyntheticPointerInput"))?;
    let destroy = GetProcAddress(user32, windows::core::s!("DestroySyntheticPointerDevice"))?;
    Some(Api {
        create: std::mem::transmute::<_, Create>(create),
        inject: std::mem::transmute::<_, Inject>(inject),
        destroy: std::mem::transmute::<_, Destroy>(destroy),
    })
});

/// Synthetic touch and pen are available on this system.
pub(crate) fn available() -> bool {
    API.is_some()
}

/// One synthetic pointer device (touch with 10 contacts, or one pen).
pub(crate) struct Device {
    handle: isize,
    kind: POINTER_INPUT_TYPE,
}

// The handle is only used by the input thread that owns the device.
unsafe impl Send for Device {}

impl Device {
    fn new(kind: POINTER_INPUT_TYPE, count: u32) -> Result<Self> {
        let Some(api) = API.as_ref() else {
            bail!("这个系统不支持合成触摸与笔（需要 Windows 10 1809 或更新）");
        };
        let handle = unsafe { (api.create)(kind, count, FEEDBACK_DEFAULT) };
        ensure!(handle != 0, "无法创建合成指针设备：{:?}", unsafe { GetLastError() });
        Ok(Self { handle, kind })
    }
    pub fn touch() -> Result<Self> {
        Self::new(PT_TOUCH, TOUCH_SLOTS)
    }
    pub fn pen() -> Result<Self> {
        Self::new(PT_PEN, 1)
    }
    fn inject(&self, frames: &[PointerTypeInfo]) -> Result<()> {
        let api = API.as_ref().expect("device exists only with the API");
        // Two calls closer than 0.1 ms may be refused as not ready.
        for attempt in 0..3 {
            if unsafe { (api.inject)(self.handle, frames.as_ptr(), frames.len() as u32) }.as_bool() {
                return Ok(());
            }
            let error = unsafe { GetLastError() };
            if error != ERROR_NOT_READY || attempt == 2 {
                bail!("注入触摸或笔失败：{error:?}");
            }
            let start = std::time::Instant::now();
            while start.elapsed() < std::time::Duration::from_micros(200) {
                std::hint::spin_loop();
            }
        }
        unreachable!()
    }
    /// One touch frame: every active contact of the desktop.
    pub fn touch_frame(&self, contacts: &[TouchPoint]) -> Result<()> {
        ensure!(self.kind == PT_TOUCH && !contacts.is_empty(), "触摸帧无效");
        let frames: Vec<_> = contacts
            .iter()
            .map(|c| PointerTypeInfo {
                kind: PT_TOUCH,
                info: PointerUnion { touch: c.info() },
            })
            .collect();
        self.inject(&frames)
    }
    pub fn pen_frame(&self, pen: &PenPoint) -> Result<()> {
        ensure!(self.kind == PT_PEN, "笔帧无效");
        self.inject(&[PointerTypeInfo {
            kind: PT_PEN,
            info: PointerUnion { pen: pen.info() },
        }])
    }
}

impl Drop for Device {
    fn drop(&mut self) {
        if let Some(api) = API.as_ref() {
            unsafe { (api.destroy)(self.handle) };
        }
    }
}

/// What a contact does in this frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Phase {
    Down,
    Update,
    Up,
    Cancel,
}

/// One touch contact in desktop pixels.
#[derive(Clone, Copy, Debug)]
pub(crate) struct TouchPoint {
    /// Windows pointer id, 0..=9.
    pub id: u32,
    pub phase: Phase,
    pub primary: bool,
    pub x: i32,
    pub y: i32,
    /// Contact area; zero = not reported.
    pub area: RECT,
    /// 0..=1024; 0 = not reported.
    pub pressure: u32,
    pub orientation: u32,
}

impl TouchPoint {
    fn info(&self) -> POINTER_TOUCH_INFO {
        let flags = match self.phase {
            Phase::Down => POINTER_FLAG_INRANGE | POINTER_FLAG_INCONTACT | POINTER_FLAG_DOWN,
            Phase::Update => POINTER_FLAG_INRANGE | POINTER_FLAG_INCONTACT | POINTER_FLAG_UPDATE,
            Phase::Up => POINTER_FLAG_UP,
            Phase::Cancel => POINTER_FLAG_UP | POINTER_FLAG_CANCELED,
        } | if self.primary {
            POINTER_FLAG_PRIMARY
        } else {
            POINTER_FLAG_NONE
        };
        let mut mask = 0;
        let area = self.area;
        if area.right > area.left && area.bottom > area.top {
            mask |= TOUCH_MASK_CONTACTAREA;
        }
        if self.pressure > 0 {
            mask |= TOUCH_MASK_PRESSURE;
        }
        if self.orientation > 0 {
            mask |= TOUCH_MASK_ORIENTATION;
        }
        POINTER_TOUCH_INFO {
            pointerInfo: POINTER_INFO {
                pointerType: PT_TOUCH,
                pointerId: self.id,
                pointerFlags: flags,
                sourceDevice: HANDLE::default(),
                ptPixelLocation: POINT { x: self.x, y: self.y },
                ..Default::default()
            },
            touchFlags: 0,
            touchMask: mask,
            rcContact: area,
            orientation: self.orientation,
            pressure: self.pressure,
            ..Default::default()
        }
    }
}

/// What the pen does in this sample.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PenAction {
    Hover,
    Down,
    Move,
    Up,
    /// Out of range.
    Leave,
    /// Cancel a contact (`UP|CANCELED`).
    Cancel,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct PenPoint {
    pub action: PenAction,
    pub x: i32,
    pub y: i32,
    /// 0..=1024.
    pub pressure: u32,
    pub rotation: u32,
    pub tilt_x: i32,
    pub tilt_y: i32,
    pub barrel: bool,
    /// The eraser end points down; `erasing` while it touches.
    pub inverted: bool,
    pub erasing: bool,
}

impl PenPoint {
    fn info(&self) -> POINTER_PEN_INFO {
        let contact = matches!(self.action, PenAction::Down | PenAction::Move);
        let flags = match self.action {
            PenAction::Hover => POINTER_FLAG_INRANGE | POINTER_FLAG_UPDATE,
            PenAction::Down => {
                POINTER_FLAG_INRANGE | POINTER_FLAG_INCONTACT | POINTER_FLAG_DOWN | POINTER_FLAG_FIRSTBUTTON
            }
            PenAction::Move => {
                POINTER_FLAG_INRANGE | POINTER_FLAG_INCONTACT | POINTER_FLAG_UPDATE | POINTER_FLAG_FIRSTBUTTON
            }
            PenAction::Up => POINTER_FLAG_INRANGE | POINTER_FLAG_UP,
            PenAction::Leave => POINTER_FLAG_UPDATE,
            PenAction::Cancel => POINTER_FLAG_INRANGE | POINTER_FLAG_UP | POINTER_FLAG_CANCELED,
        } | POINTER_FLAG_PRIMARY;
        let button = match self.action {
            PenAction::Down => POINTER_CHANGE_FIRSTBUTTON_DOWN,
            PenAction::Up | PenAction::Cancel => POINTER_CHANGE_FIRSTBUTTON_UP,
            _ => POINTER_CHANGE_NONE,
        };
        let mut pen_flags = 0;
        if self.barrel && self.action != PenAction::Leave {
            pen_flags |= PEN_FLAG_BARREL;
        }
        if self.inverted && self.action != PenAction::Leave {
            pen_flags |= PEN_FLAG_INVERTED;
            if contact && self.erasing {
                pen_flags |= PEN_FLAG_ERASER;
            }
        }
        POINTER_PEN_INFO {
            pointerInfo: POINTER_INFO {
                pointerType: PT_PEN,
                pointerId: 0,
                pointerFlags: flags,
                ptPixelLocation: POINT { x: self.x, y: self.y },
                ButtonChangeType: button,
                ..Default::default()
            },
            penFlags: pen_flags,
            penMask: PEN_MASK_PRESSURE | PEN_MASK_ROTATION | PEN_MASK_TILT_X | PEN_MASK_TILT_Y,
            pressure: if contact { self.pressure.min(1024) } else { 0 },
            rotation: self.rotation % 360,
            tiltX: self.tilt_x.clamp(-90, 90),
            tiltY: self.tilt_y.clamp(-90, 90),
        }
    }
}
