//! Temporary OLE handoff target on the remote user desktop. Only a real native
//! file drag can supply paths. It is kept alive for a negotiated return; copies
//! and cancellations end it at the NONE target rather than at an arbitrary app.
use super::*;
use std::{
    cell::Cell,
    path::PathBuf,
    rc::Rc,
    time::{Duration, Instant},
};
pub(crate) enum Event {
    Unavailable,
    Captured(Vec<PathBuf>, bool),
    Resumed(super::original::Original),
    Released,
    Failed(String),
}
pub(crate) struct Portal {
    stop: Arc<AtomicBool>,
    resume: Arc<Mutex<Option<Position>>>,
}
impl Portal {
    pub fn start(
        point: Position,
        preserve: bool,
        allowed: Arc<dyn Fn() -> bool + Send + Sync>,
        notify: Arc<dyn Fn(Event) + Send + Sync>,
    ) -> Result<Self> {
        let stop = Arc::new(AtomicBool::new(false));
        let ending = stop.clone();
        let resume = Arc::new(Mutex::new(None));
        let returning = resume.clone();
        std::thread::Builder::new()
            .name("OLE file handoff".into())
            .spawn(move || {
                match run(point, preserve, returning, ending, allowed, notify.clone()) {
                    Ok(event) => notify(event),
                    Err(error) => notify(Event::Failed(error.to_string())),
                }
            })?;
        Ok(Self { stop, resume })
    }
    pub fn resume(&self, point: Position) {
        *super::super::lock(&self.resume) = Some(point);
    }
}
impl Drop for Portal {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
    }
}
struct Handler {
    hwnd: HWND,
    allowed: Arc<dyn Fn() -> bool + Send + Sync>,
    stop: Arc<AtomicBool>,
    captured: Rc<Cell<bool>>,
    left: Rc<Cell<bool>>,
    dropped: Rc<Cell<bool>>,
    notify: Arc<dyn Fn(Event) + Send + Sync>,
    original: Option<super::original::Original>,
}
impl target::Handler for Handler {
    fn enter(&mut self, paths: Vec<PathBuf>, _: POINTL) -> u32 {
        if self.stop.load(Ordering::Acquire) || !(self.allowed)() {
            return 0;
        }
        self.left.set(false);
        if !self.captured.replace(true) {
            // Once the file selection is known, every mouse-up must end at our
            // NONE target, including after a screen move during the handoff.
            if cover_desktop(self.hwnd).is_err() {
                return 0;
            }
            (self.notify)(Event::Captured(
                paths,
                self.original.is_some_and(|v| v.live()),
            ));
        }
        DROPEFFECT_COPY.0
    }
    fn over(&mut self, _: POINTL) -> u32 {
        if self.stop.load(Ordering::Acquire) || !(self.allowed)() {
            0
        } else {
            DROPEFFECT_COPY.0
        }
    }
    fn leave(&mut self) {
        if self.captured.get() {
            self.left.set(true);
        }
    }
    fn drop_at(&mut self, _: POINTL) -> u32 {
        self.dropped
            .set(!self.stop.load(Ordering::Acquire) && (self.allowed)());
        self.left.set(true);
        DROPEFFECT_NONE.0
    }
}
fn cover_desktop(hwnd: HWND) -> Result<()> {
    unsafe {
        SetWindowPos(
            hwnd,
            Some(HWND_TOPMOST),
            GetSystemMetrics(SM_XVIRTUALSCREEN),
            GetSystemMetrics(SM_YVIRTUALSCREEN),
            GetSystemMetrics(SM_CXVIRTUALSCREEN).max(1),
            GetSystemMetrics(SM_CYVIRTUALSCREEN).max(1),
            SWP_NOACTIVATE | SWP_SHOWWINDOW,
        )?;
    }
    Ok(())
}
unsafe extern "system" fn procedure(h: HWND, m: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    unsafe { DefWindowProcW(h, m, w, l) }
}
fn run(
    point: Position,
    preserve: bool,
    resume: Arc<Mutex<Option<Position>>>,
    stop: Arc<AtomicBool>,
    allowed: Arc<dyn Fn() -> bool + Send + Sync>,
    notify: Arc<dyn Fn(Event) + Send + Sync>,
) -> Result<Event> {
    ensure!(allowed(), "拖出未获许可");
    let original = preserve
        .then(super::original::Original::foreground)
        .flatten();
    let old = unsafe { SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
    struct Dpi(DPI_AWARENESS_CONTEXT);
    impl Drop for Dpi {
        fn drop(&mut self) {
            unsafe {
                SetThreadDpiAwarenessContext(self.0);
            }
        }
    }
    let _dpi = Dpi(old);
    let class = windows::core::w!("Farwing.FileHandoff");
    let module = unsafe { GetModuleHandleW(None)? };
    unsafe {
        RegisterClassW(&WNDCLASSW {
            lpfnWndProc: Some(procedure),
            hInstance: module.into(),
            lpszClassName: class,
            ..Default::default()
        });
    }
    let hwnd = unsafe {
        CreateWindowExW(
            WS_EX_TOPMOST | WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW | WS_EX_LAYERED,
            class,
            windows::core::w!(""),
            WS_POPUP,
            point.x - 8,
            point.y - 8,
            17,
            17,
            None,
            None,
            Some(module.into()),
            None,
        )?
    };
    struct Window(HWND);
    impl Drop for Window {
        fn drop(&mut self) {
            unsafe {
                let _ = DestroyWindow(self.0);
            }
        }
    }
    let _window = Window(hwnd);
    let captured = Rc::new(Cell::new(false));
    let left = Rc::new(Cell::new(false));
    let dropped = Rc::new(Cell::new(false));
    let handler = Rc::new(RefCell::new(Handler {
        hwnd,
        allowed: allowed.clone(),
        stop: stop.clone(),
        captured: captured.clone(),
        left: left.clone(),
        dropped: dropped.clone(),
        notify: notify.clone(),
        original,
    }));
    let _registration = target::Registration::new(hwnd, handler)?;
    unsafe {
        SetLayeredWindowAttributes(hwnd, COLORREF(0), 1, LWA_ALPHA)?;
        let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
        SetCursorPos(point.x, point.y)?;
    }
    let deadline = Instant::now() + Duration::from_millis(300);
    let mut released_at: Option<Instant> = None;
    let mut raised_after_leave = false;
    let mut cancelling: Option<Instant> = None;
    loop {
        if stop.load(Ordering::Acquire) || !allowed() {
            if !captured.get() {
                return Ok(Event::Unavailable);
            }
            if cancelling.is_none() {
                if let Some(original) = original {
                    original.cancel()?;
                }
                cancelling = Some(Instant::now());
            }
        }
        let mut msg = MSG::default();
        while unsafe { PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() } {
            unsafe {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
        let held = unsafe { GetAsyncKeyState(VK_LBUTTON.0 as i32) } < 0;
        if captured.get() {
            if !held && dropped.get() {
                return Ok(Event::Released);
            }
            if let Some(original) = original {
                if !original.live() && (held || cancelling.is_some()) {
                    return Ok(Event::Unavailable);
                }
            }
            if let Some(since) = cancelling {
                ensure!(since.elapsed() < Duration::from_secs(1), "原拖动取消未确认");
            } else if let Some(position) = super::super::lock(&resume).take() {
                let original = original
                    .filter(|v| v.live())
                    .ok_or_else(|| anyhow::anyhow!("原拖动已结束"))?;
                ensure!(held, "原鼠标按下已释放");
                unsafe {
                    let _ = ShowWindow(hwnd, SW_HIDE);
                    SetCursorPos(position.x, position.y)?;
                }
                // Notify only after run returns and Registration/Window are gone.
                return Ok(Event::Resumed(original));
            }
            if !held {
                if dropped.get() {
                    return Ok(Event::Released);
                }
                ensure!(!left.get(), "原文件拖动已取消");
                // The hardware UP can precede OLE's cross-thread Drop call.
                // Keep the NONE target until that call or its bounded timeout.
                let released = released_at.get_or_insert_with(Instant::now);
                ensure!(
                    released.elapsed() < Duration::from_millis(500),
                    "原文件拖动未确认结束"
                );
            } else {
                released_at = None;
                let mut rect = RECT::default();
                unsafe {
                    GetWindowRect(hwnd, &mut rect)?;
                }
                let bounds = unsafe {
                    (
                        GetSystemMetrics(SM_XVIRTUALSCREEN),
                        GetSystemMetrics(SM_YVIRTUALSCREEN),
                        GetSystemMetrics(SM_CXVIRTUALSCREEN),
                        GetSystemMetrics(SM_CYVIRTUALSCREEN),
                    )
                };
                if !left.get() {
                    raised_after_leave = false;
                }
                if (left.get() && !raised_after_leave)
                    || (
                        rect.left,
                        rect.top,
                        rect.right - rect.left,
                        rect.bottom - rect.top,
                    ) != bounds
                {
                    cover_desktop(hwnd)?;
                    raised_after_leave = true;
                }
            }
        } else if !allowed() || stop.load(Ordering::Acquire) || Instant::now() > deadline {
            return Ok(Event::Unavailable);
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}
