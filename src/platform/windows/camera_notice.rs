//! The corner notice on the host while its camera is watched
//! (`docs/protocol/camera-design.md` §5): who watches, and a button that
//! stops it. The window is excluded from capture, so the remote viewers of
//! the screen never see it. One thread owns it while it is shown.
use std::sync::{Mutex, OnceLock};
use std::time::Duration;
use windows::{
    Win32::{
        Foundation::*,
        Graphics::Gdi::*,
        System::LibraryLoader::GetModuleHandleW,
        UI::{HiDpi::*, WindowsAndMessaging::*},
    },
    core::{PCWSTR, w},
};

const CLASS: PCWSTR = w!("Farwing.Camera.Notice");
// Geometry in DIPs.
const WIDTH: i32 = 320;
const HEIGHT: i32 = 44;
const MARGIN: i32 = 16;
const RADIUS: i32 = 10;
const BUTTON: (i32, i32) = (64, 28);
const PADDING: i32 = 14;
const FONT: i32 = 14;
const BACKGROUND: u32 = 0x00_2a_24_20; // BGR of #20242a
const TEXT: u32 = 0x00_f0_ea_e6;
const BUTTON_FILL: u32 = 0x00_44_3c_d9; // #d93c44
const BUTTON_TEXT: u32 = 0x00_ff_ff_ff;

#[derive(Default)]
struct State {
    /// The text to show; `None` hides the notice.
    text: Option<String>,
    button: String,
    running: bool,
}

static STATE: Mutex<State> = Mutex::new(State {
    text: None,
    button: String::new(),
    running: false,
});
static ON_STOP: OnceLock<fn()> = OnceLock::new();

fn state() -> std::sync::MutexGuard<'static, State> {
    STATE.lock().unwrap_or_else(|e| e.into_inner())
}

/// Show `text` with a `button` that calls `on_stop`; `None` hides it.
pub(crate) fn set(text: Option<String>, button: &str, on_stop: fn()) {
    let _ = ON_STOP.set(on_stop);
    let mut state = state();
    state.text = text;
    button.clone_into(&mut state.button);
    if state.text.is_some() && !state.running {
        state.running = true;
        if std::thread::Builder::new()
            .name("camera-notice".into())
            .spawn(run)
            .is_err()
        {
            state.running = false;
        }
    }
}

fn run() {
    let mut window = None::<Window>;
    loop {
        let (text, button) = {
            let mut state = state();
            match state.text.clone() {
                Some(text) => (text, state.button.clone()),
                None => {
                    state.running = false;
                    return;
                }
            }
        };
        if window.as_ref().is_none_or(|w| w.text != text || w.button != button) {
            drop(window.take());
            match Window::new(&text, &button) {
                Ok(created) => {
                    tracing::info!("camera notice shown");
                    window = Some(created);
                }
                Err(error) => {
                    tracing::warn!(%error, "camera notice not shown");
                    state().running = false;
                    return;
                }
            }
        }
        unsafe {
            let mut message = MSG::default();
            while PeekMessageW(&mut message, None, 0, 0, PM_REMOVE).as_bool() {
                let _ = TranslateMessage(&message);
                DispatchMessageW(&message);
            }
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

struct Window {
    hwnd: HWND,
    text: String,
    button: String,
}

/// The window's texts for painting (one notice at a time).
static PAINTED: Mutex<(String, String)> = Mutex::new((String::new(), String::new()));

impl Window {
    fn new(text: &str, button: &str) -> windows::core::Result<Self> {
        *PAINTED.lock().unwrap_or_else(|e| e.into_inner()) = (text.to_owned(), button.to_owned());
        unsafe {
            let _ = SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
            static REGISTERED: OnceLock<bool> = OnceLock::new();
            REGISTERED.get_or_init(|| {
                let class = WNDCLASSW {
                    lpfnWndProc: Some(window_proc),
                    hInstance: GetModuleHandleW(None).unwrap_or_default().into(),
                    lpszClassName: CLASS,
                    hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
                    ..Default::default()
                };
                RegisterClassW(&class) != 0
            });
            let mut area = RECT::default();
            let _ = SystemParametersInfoW(
                SPI_GETWORKAREA,
                0,
                Some((&raw mut area).cast()),
                SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
            );
            let primary = MonitorFromPoint(POINT { x: area.left, y: area.top }, MONITOR_DEFAULTTOPRIMARY);
            let (mut dpi, mut unused) = (96u32, 96u32);
            let _ = GetDpiForMonitor(primary, MDT_EFFECTIVE_DPI, &mut dpi, &mut unused);
            let scale = |v: i32| v * dpi as i32 / 96;
            let (width, height) = (scale(WIDTH), scale(HEIGHT));
            let hwnd = CreateWindowExW(
                WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
                CLASS,
                w!("Farwing"),
                WS_POPUP,
                area.right - width - scale(MARGIN),
                area.bottom - height - scale(MARGIN),
                width,
                height,
                None,
                None,
                Some(GetModuleHandleW(None)?.into()),
                None,
            )?;
            // Before it is ever shown: the screen's viewers must not see it.
            let _ = SetWindowDisplayAffinity(hwnd, WDA_EXCLUDEFROMCAPTURE);
            let region = CreateRoundRectRgn(0, 0, width + 1, height + 1, scale(RADIUS), scale(RADIUS));
            SetWindowRgn(hwnd, Some(region), false);
            let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
            Ok(Self {
                hwnd,
                text: text.to_owned(),
                button: button.to_owned(),
            })
        }
    }
}

impl Drop for Window {
    fn drop(&mut self) {
        unsafe {
            let _ = DestroyWindow(self.hwnd);
        }
    }
}

/// The stop button's rectangle in a window of `client` size.
fn button_rect(client: &RECT, dpi: u32) -> RECT {
    let scale = |v: i32| v * dpi as i32 / 96;
    let (width, height) = (scale(BUTTON.0), scale(BUTTON.1));
    let right = client.right - scale(PADDING) / 2 - scale(4);
    let top = (client.bottom - height) / 2;
    RECT {
        left: right - width,
        top,
        right,
        bottom: top + height,
    }
}

unsafe extern "system" fn window_proc(hwnd: HWND, message: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match message {
        WM_MOUSEACTIVATE => LRESULT(MA_NOACTIVATE as isize),
        WM_LBUTTONUP => {
            let dpi = unsafe { GetDpiForWindow(hwnd) };
            let mut client = RECT::default();
            let _ = unsafe { GetClientRect(hwnd, &mut client) };
            let at = POINT {
                x: (lparam.0 & 0xffff) as i16 as i32,
                y: ((lparam.0 >> 16) & 0xffff) as i16 as i32,
            };
            if unsafe { PtInRect(&button_rect(&client, dpi), at) }.as_bool()
                && let Some(stop) = ON_STOP.get()
            {
                stop();
            }
            LRESULT(0)
        }
        WM_PAINT => {
            let mut paint = PAINTSTRUCT::default();
            unsafe {
                let dc = BeginPaint(hwnd, &mut paint);
                let dpi = GetDpiForWindow(hwnd);
                let scale = |v: i32| v * dpi as i32 / 96;
                let mut client = RECT::default();
                let _ = GetClientRect(hwnd, &mut client);
                let background = CreateSolidBrush(COLORREF(BACKGROUND));
                FillRect(dc, &client, background);
                let _ = DeleteObject(background.into());
                let font = CreateFontW(
                    -scale(FONT),
                    0,
                    0,
                    0,
                    FW_NORMAL.0 as i32,
                    0,
                    0,
                    0,
                    DEFAULT_CHARSET,
                    OUT_DEFAULT_PRECIS,
                    CLIP_DEFAULT_PRECIS,
                    CLEARTYPE_QUALITY,
                    0,
                    w!("Microsoft YaHei UI"),
                );
                let old = SelectObject(dc, font.into());
                SetBkMode(dc, TRANSPARENT);
                let (text, button) = PAINTED.lock().unwrap_or_else(|e| e.into_inner()).clone();
                let stop = button_rect(&client, dpi);
                let mut label = RECT {
                    left: scale(PADDING),
                    top: 0,
                    right: stop.left - scale(8),
                    bottom: client.bottom,
                };
                SetTextColor(dc, COLORREF(TEXT));
                let mut wide: Vec<u16> = text.encode_utf16().collect();
                DrawTextW(dc, &mut wide, &mut label, DT_SINGLELINE | DT_VCENTER | DT_END_ELLIPSIS | DT_NOPREFIX);
                let fill = CreateSolidBrush(COLORREF(BUTTON_FILL));
                let pen = SelectObject(dc, GetStockObject(NULL_PEN));
                let brush = SelectObject(dc, fill.into());
                let _ = RoundRect(dc, stop.left, stop.top, stop.right, stop.bottom, scale(6), scale(6));
                SelectObject(dc, brush);
                SelectObject(dc, pen);
                let _ = DeleteObject(fill.into());
                SetTextColor(dc, COLORREF(BUTTON_TEXT));
                let mut wide: Vec<u16> = button.encode_utf16().collect();
                let mut stop_text = stop;
                DrawTextW(dc, &mut wide, &mut stop_text, DT_SINGLELINE | DT_VCENTER | DT_CENTER | DT_NOPREFIX);
                SelectObject(dc, old);
                let _ = DeleteObject(font.into());
                let _ = EndPaint(hwnd, &paint);
            }
            LRESULT(0)
        }
        WM_ERASEBKGND => LRESULT(1),
        _ => unsafe { DefWindowProcW(hwnd, message, wparam, lparam) },
    }
}
