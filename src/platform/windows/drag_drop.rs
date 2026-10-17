//! User-desktop OLE drag source. Transport and file ownership stay with callers.
//! Only this STA's OLE capture window receives synthetic loop notifications;
//! we do not press a mouse button in an arbitrary destination application.
pub(crate) mod appearance;
pub(crate) mod original;
pub(crate) mod portal;
pub(crate) mod send_target;
pub(crate) mod target;
use anyhow::{Result, ensure};
use std::{
    cell::RefCell,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering},
    },
};
use windows::{
    Win32::{
        Foundation::*,
        Graphics::Gdi::ScreenToClient,
        System::{
            Com::IDataObject,
            LibraryLoader::GetModuleHandleW,
            Ole::*,
            SystemServices::MODIFIERKEYS_FLAGS,
            Threading::{CreateMutexW, GetCurrentThreadId, ReleaseMutex, WaitForSingleObject},
        },
        UI::{
            HiDpi::*,
            Input::KeyboardAndMouse::{GetAsyncKeyState, GetCapture, VK_LBUTTON, VK_RBUTTON},
            WindowsAndMessaging::*,
        },
    },
    core::{BOOL, HRESULT, implement},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Position {
    pub x: i32,
    pub y: i32,
}
/// The OLE loop of a drag handed off to the peer can be resumed when the
/// drag re-enters the viewer (`NATIVE_RETURN`).
pub(crate) const RETURN_CAPABLE: bool = true;
pub(crate) fn left_held() -> bool {
    unsafe { GetAsyncKeyState(VK_LBUTTON.0 as i32) < 0 }
}
pub(crate) fn wake_viewer(owner: u64) {
    let hwnd = HWND(owner as _);
    let mut point = POINT::default();
    unsafe {
        let mut process = 0;
        GetWindowThreadProcessId(hwnd, Some(&mut process));
        if process != windows::Win32::System::Threading::GetCurrentProcessId()
            || GetForegroundWindow() != hwnd
        {
            return;
        }
        if GetCursorPos(&mut point).is_ok() && ScreenToClient(hwnd, &mut point).as_bool() {
            let xy = LPARAM(((point.x as u16 as u32) | ((point.y as u16 as u32) << 16)) as isize);
            let _ = PostMessageW(Some(hwnd), WM_MOUSEMOVE, WPARAM(0), xy);
        }
    }
}
#[derive(Clone, Copy, PartialEq, Eq)]
enum Phase {
    Dragging,
    Drop,
    Cancel,
}
struct Desired {
    point: Position,
    phase: Phase,
    serial: u64,
}
pub(crate) enum Event {
    Feedback(u32),
    Released,
    Finished {
        accepted: bool,
        effect: u32,
        error: Option<String>,
    },
}
struct Shared {
    desired: Mutex<Desired>,
    physical: bool,
    allowed: Arc<dyn Fn() -> bool + Send + Sync>,
    notify: Arc<dyn Fn(Event) + Send + Sync>,
    finished: AtomicBool,
    applied: AtomicU64,
    released: AtomicBool,
    feedback: AtomicU32,
}
impl Shared {
    fn allowed(&self) -> bool {
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| (self.allowed)())).unwrap_or(false)
    }
    fn emit(&self, event: Event) {
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| (self.notify)(event)));
    }
}
/// Windows 上没有这条路。那边的拖放全是 OLE：由正在拖的那一方把数据交出去
/// （见 `portal`），不存在「在指针底下摆个窗口去接住别人的拖放」这件事，也就
/// 没有对应能力。它是个无值类型：共享代码两侧形状一致、调用处不必分平台，
/// 却造不出一个假装能用的接取者。
pub(crate) enum Catcher {}
impl Catcher {
    pub fn start(
        _device: &str,
        _size: (u32, u32),
        _point: Position,
        _allowed: Arc<dyn Fn() -> bool + Send + Sync>,
        _notify: Arc<dyn Fn(portal::Event) + Send + Sync>,
    ) -> Result<Self> {
        anyhow::bail!("Windows 不在指针底下接取拖放")
    }
    pub fn cancel(&self) {
        match *self {}
    }
}

pub(crate) struct Session {
    shared: Arc<Shared>,
}
impl Session {
    pub fn start(
        point: Position,
        allowed: Arc<dyn Fn() -> bool + Send + Sync>,
        object: impl FnOnce() -> Result<IDataObject> + Send + 'static,
        notify: Arc<dyn Fn(Event) + Send + Sync>,
    ) -> Result<Self> {
        Self::start_mode(point, false, allowed, object, notify)
    }
    pub fn physical(
        allowed: Arc<dyn Fn() -> bool + Send + Sync>,
        object: impl FnOnce() -> Result<IDataObject> + Send + 'static,
        notify: Arc<dyn Fn(Event) + Send + Sync>,
    ) -> Result<Self> {
        let mut point = POINT::default();
        unsafe {
            GetCursorPos(&mut point)?;
        }
        ensure!(
            unsafe { GetAsyncKeyState(VK_LBUTTON.0 as i32) } < 0,
            "鼠标已松开，拖出已取消"
        );
        Self::start_mode(
            Position {
                x: point.x,
                y: point.y,
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
        object: impl FnOnce() -> Result<IDataObject> + Send + 'static,
        notify: Arc<dyn Fn(Event) + Send + Sync>,
    ) -> Result<Self> {
        let shared = Arc::new(Shared {
            physical,
            desired: Mutex::new(Desired {
                point,
                phase: Phase::Dragging,
                serial: 1,
            }),
            allowed,
            notify,
            finished: AtomicBool::new(false),
            applied: AtomicU64::new(0),
            released: AtomicBool::new(false),
            feedback: AtomicU32::new(u32::MAX),
        });
        ensure!(shared.allowed(), "拖放未获许可");
        let running = shared.clone();
        std::thread::Builder::new()
            .name("OLE remote drag".into())
            .spawn(move || {
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    run(running.clone(), object)
                }));
                running.finished.store(true, Ordering::Release);
                match result {
                    Ok(Ok((accepted, effect))) => running.emit(Event::Finished {
                        accepted,
                        effect,
                        error: None,
                    }),
                    Ok(Err(error)) => running.emit(Event::Finished {
                        accepted: false,
                        effect: 0,
                        error: Some(error.to_string()),
                    }),
                    Err(_) => running.emit(Event::Finished {
                        accepted: false,
                        effect: 0,
                        error: Some("拖放执行线程已停止".into()),
                    }),
                }
            })?;
        Ok(Self { shared })
    }
    pub fn position(&self, point: Position) {
        let mut desired = super::lock(&self.shared.desired);
        if desired.phase == Phase::Dragging {
            desired.point = point;
            desired.serial = desired.serial.wrapping_add(1);
        }
    }
    pub fn commit(&self, point: Position) -> Result<()> {
        let mut desired = super::lock(&self.shared.desired);
        ensure!(
            desired.phase == Phase::Dragging && !self.shared.finished.load(Ordering::Acquire),
            "拖放已结束"
        );
        desired.point = point;
        desired.phase = Phase::Drop;
        desired.serial = desired.serial.wrapping_add(1);
        Ok(())
    }
    pub fn cancel(&self) {
        let mut desired = super::lock(&self.shared.desired);
        desired.phase = Phase::Cancel;
        desired.serial = desired.serial.wrapping_add(1);
    }
}
impl Drop for Session {
    fn drop(&mut self) {
        self.cancel();
    }
}

#[implement(IDropSource)]
struct Source {
    shared: Arc<Shared>,
}
impl IDropSource_Impl for Source_Impl {
    fn QueryContinueDrag(&self, escape: BOOL, _: MODIFIERKEYS_FLAGS) -> HRESULT {
        if escape.as_bool() || !self.shared.allowed() {
            return DRAGDROP_S_CANCEL;
        }
        let desired = super::lock(&self.shared.desired);
        if self.shared.physical && desired.phase != Phase::Cancel {
            if unsafe { GetAsyncKeyState(VK_RBUTTON.0 as i32) } < 0 {
                return DRAGDROP_S_CANCEL;
            }
            if unsafe { GetAsyncKeyState(VK_LBUTTON.0 as i32) } >= 0 {
                if !self.shared.released.swap(true, Ordering::AcqRel) {
                    self.shared.emit(Event::Released);
                }
                return DRAGDROP_S_DROP;
            }
            return S_OK;
        }
        match desired.phase {
            Phase::Dragging => S_OK,
            Phase::Drop if self.shared.applied.load(Ordering::Acquire) == desired.serial => {
                let mut point = POINT::default();
                if unsafe { GetCursorPos(&mut point) }.is_ok()
                    && point.x == desired.point.x
                    && point.y == desired.point.y
                {
                    if !self.shared.released.swap(true, Ordering::AcqRel) {
                        self.shared.emit(Event::Released);
                    }
                    DRAGDROP_S_DROP
                } else {
                    DRAGDROP_S_CANCEL
                }
            }
            Phase::Drop => S_OK,
            Phase::Cancel => DRAGDROP_S_CANCEL,
        }
    }
    fn GiveFeedback(&self, effect: DROPEFFECT) -> HRESULT {
        if self.shared.feedback.swap(effect.0, Ordering::AcqRel) != effect.0 {
            self.shared.emit(Event::Feedback(effect.0));
        }
        DRAGDROP_S_USEDEFAULTCURSORS
    }
}
struct Pump {
    shared: Arc<Shared>,
    applied: u64,
}
thread_local! { static PUMP: RefCell<Option<Pump>> = const { RefCell::new(None) }; }
const TICK: usize = 1;
fn tick() {
    let Some(shared) = PUMP.with(|slot| slot.borrow().as_ref().map(|p| p.shared.clone())) else {
        return;
    };
    let allowed = shared.allowed();
    let update = PUMP.with(|slot| {
        let mut slot = slot.borrow_mut();
        let pump = slot.as_mut()?;
        let mut desired = super::lock(&pump.shared.desired);
        if !allowed {
            desired.phase = Phase::Cancel;
            desired.serial = desired.serial.wrapping_add(1);
        }
        if shared.physical {
            let mut point = POINT::default();
            if unsafe { GetCursorPos(&mut point) }.is_ok() {
                desired.point = Position {
                    x: point.x,
                    y: point.y,
                };
            }
            desired.serial = desired.serial.wrapping_add(1);
        }
        if desired.serial == pump.applied {
            return None;
        }
        pump.applied = desired.serial;
        Some((desired.point, desired.phase, desired.serial))
    });
    let Some((point, phase, serial)) = update else {
        return;
    };
    unsafe {
        if !shared.physical && phase != Phase::Cancel && SetCursorPos(point.x, point.y).is_err() {
            let mut desired = super::lock(&shared.desired);
            desired.phase = Phase::Cancel;
            desired.serial = desired.serial.wrapping_add(1);
        }
        let capture = GetCapture();
        if capture.is_invalid() || GetWindowThreadProcessId(capture, None) != GetCurrentThreadId() {
            return;
        }
        let mut client = POINT {
            x: point.x,
            y: point.y,
        };
        if !ScreenToClient(capture, &mut client).as_bool() {
            return;
        }
        let xy = LPARAM(((client.x as u16 as u32) | ((client.y as u16 as u32) << 16)) as isize);
        shared.applied.store(serial, Ordering::Release);
        let _ = PostMessageW(Some(capture), WM_MOUSEMOVE, WPARAM(0), xy);
        if phase != Phase::Dragging {
            let _ = PostMessageW(Some(capture), WM_LBUTTONUP, WPARAM(0), xy);
        }
    }
}
unsafe extern "system" fn window_proc(hwnd: HWND, msg: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    if msg == WM_TIMER {
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(tick));
        return LRESULT(0);
    }
    unsafe { DefWindowProcW(hwnd, msg, w, l) }
}
fn run(shared: Arc<Shared>, object: impl FnOnce() -> Result<IDataObject>) -> Result<(bool, u32)> {
    // One source owns the user desktop at a time, including separate agent
    // processes. Wait only for a retiring source; never preempt another drag.
    struct DesktopLock {
        handle: HANDLE,
        owned: bool,
    }
    impl Drop for DesktopLock {
        fn drop(&mut self) {
            unsafe {
                if self.owned {
                    let _ = ReleaseMutex(self.handle);
                }
                let _ = CloseHandle(self.handle);
            }
        }
    }
    let mut desktop = DesktopLock {
        handle: unsafe {
            CreateMutexW(
                None,
                false,
                windows::core::w!("Local\\OpenUUYC.NativeFileDrag"),
            )?
        },
        owned: false,
    };
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
    loop {
        ensure!(
            shared.allowed() && super::lock(&shared.desired).phase != Phase::Cancel,
            "拖放已取消"
        );
        let result = unsafe { WaitForSingleObject(desktop.handle, 10) };
        if result == WAIT_OBJECT_0 || result == WAIT_ABANDONED {
            desktop.owned = true;
            break;
        }
        ensure!(
            result == WAIT_TIMEOUT && std::time::Instant::now() < deadline,
            "桌面正在处理另一个拖放"
        );
    }
    let previous_dpi =
        unsafe { SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
    struct Dpi(DPI_AWARENESS_CONTEXT);
    impl Drop for Dpi {
        fn drop(&mut self) {
            unsafe {
                SetThreadDpiAwarenessContext(self.0);
            }
        }
    }
    let _dpi = Dpi(previous_dpi);
    unsafe {
        OleInitialize(None)?;
    }
    struct Ole;
    impl Drop for Ole {
        fn drop(&mut self) {
            unsafe {
                OleUninitialize();
            }
        }
    }
    let _ole = Ole;
    let object = object()?;
    ensure!(shared.allowed(), "拖放许可已撤销");
    let class = windows::core::w!("OpenUUYC.DragSource");
    let module = unsafe { GetModuleHandleW(None)? };
    let wc = WNDCLASSW {
        lpfnWndProc: Some(window_proc),
        hInstance: module.into(),
        lpszClassName: class,
        ..Default::default()
    };
    unsafe {
        RegisterClassW(&wc);
    }
    let hwnd = unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE(0),
            class,
            windows::core::w!(""),
            WINDOW_STYLE(0),
            0,
            0,
            0,
            0,
            Some(HWND_MESSAGE),
            None,
            Some(module.into()),
            None,
        )?
    };
    struct PumpWindow(HWND);
    impl Drop for PumpWindow {
        fn drop(&mut self) {
            unsafe {
                let _ = KillTimer(Some(self.0), TICK);
                let _ = DestroyWindow(self.0);
            }
            PUMP.with(|s| s.borrow_mut().take());
        }
    }
    let _window = PumpWindow(hwnd);
    PUMP.with(|s| {
        *s.borrow_mut() = Some(Pump {
            shared: shared.clone(),
            applied: 0,
        })
    });
    ensure!(
        unsafe { SetTimer(Some(hwnd), TICK, 8, None) } != 0,
        "无法启动拖放消息泵"
    );
    let source: IDropSource = Source { shared }.into();
    // OLE obtains its initial mouse position/state from a mouse message in the
    // calling STA. A background drag worker has no such input queue history.
    // Seed its own helper window; do not inject a button into another app.
    let first = PUMP
        .with(|s| {
            s.borrow()
                .as_ref()
                .map(|p| super::lock(&p.shared.desired).point)
        })
        .unwrap();
    unsafe {
        if !PUMP.with(|s| s.borrow().as_ref().unwrap().shared.physical) {
            SetCursorPos(first.x, first.y)?;
        } else {
            ensure!(
                GetAsyncKeyState(VK_LBUTTON.0 as i32) < 0,
                "鼠标已松开，拖出已取消"
            );
        }
        let mut point = POINT {
            x: first.x,
            y: first.y,
        };
        ensure!(
            ScreenToClient(hwnd, &mut point).as_bool(),
            "无法转换拖放起点"
        );
        let xy = LPARAM(((point.x as u16 as u32) | ((point.y as u16 as u32) << 16)) as isize);
        PostMessageW(Some(hwnd), WM_MOUSEMOVE, WPARAM(1), xy)?;
    }
    let mut effect = DROPEFFECT_NONE;
    let result = unsafe { DoDragDrop(&object, &source, DROPEFFECT_COPY, &mut effect) };
    result.ok()?;
    Ok((
        result == DRAGDROP_S_DROP
            && effect.0 & DROPEFFECT_COPY.0 != 0
            && effect.0 & (DROPEFFECT_MOVE.0 | DROPEFFECT_LINK.0) == 0,
        effect.0 & DROPEFFECT_COPY.0,
    ))
}
