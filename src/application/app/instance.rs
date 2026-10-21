//! One control center per Windows login session, independent of the EXE name.
use crate::product::{self, WideName};
use anyhow::{Context, Result};
use windows::Win32::Foundation::{
    CloseHandle, ERROR_ALREADY_EXISTS, GetLastError, HANDLE, HWND, LPARAM, LRESULT, WPARAM,
};
use windows::Win32::System::Threading::CreateMutexW;
use windows::Win32::UI::Shell::{DefSubclassProc, RemoveWindowSubclass, SetWindowSubclass};
use windows::Win32::UI::WindowsAndMessaging::{
    AllowSetForegroundWindow, EnumWindows, GetPropW, GetWindowThreadProcessId, MB_ICONERROR, MB_OK,
    MB_SETFOREGROUND, MessageBoxW, PostMessageW, RegisterWindowMessageW, RemovePropW, SetPropW,
    WM_NCDESTROY,
};
use windows::core::{BOOL, PCWSTR, w};

pub struct Instance(HANDLE);
const SHOW_SUBCLASS: usize = 0x4f554943;
const NOTIFICATION_MESSAGE: usize = 0x4f554e54;
// Every name below is profile-specific, so profiles never block, activate,
// close or update each other's control center.
static UPDATING: WideName = WideName::new(|| product::local_object("Updating.v1"));
static CONTROL_CENTER: WideName = WideName::new(|| product::local_object("ControlCenter.v1"));
static INSTALLER: WideName = WideName::new(|| product::local_object("Installer.v1"));
static WINDOW_PROPERTY: WideName =
    WideName::new(|| product::instance_atom("ControlCenter.Window.v1"));
static SHOW_MESSAGE: WideName = WideName::new(|| product::instance_atom("ControlCenter.Show.v1"));
static EXIT_MESSAGE: WideName =
    WideName::new(|| product::instance_atom("ControlCenter.ExitForUpdate.v1"));
static UPDATE_EXIT: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
pub(crate) fn take_update_exit() -> bool {
    UPDATE_EXIT.swap(false, std::sync::atomic::Ordering::AcqRel)
}

fn show_message() -> u32 {
    static MESSAGE: std::sync::OnceLock<u32> = std::sync::OnceLock::new();
    *MESSAGE.get_or_init(|| unsafe { RegisterWindowMessageW(SHOW_MESSAGE.pcwstr()) })
}

impl Drop for Instance {
    fn drop(&mut self) {
        let _ = unsafe { CloseHandle(self.0) };
    }
}

fn acquire_named(name: PCWSTR) -> Result<Option<Instance>> {
    // Object lifetime is the reservation; no thread ownership or abandoned lock remains.
    let handle = unsafe { CreateMutexW(None, false, name) }.context("创建程序实例保护失败")?;
    let exists = unsafe { GetLastError() } == ERROR_ALREADY_EXISTS;
    let instance = Instance(handle);
    if exists {
        drop(instance);
        Ok(None)
    } else {
        Ok(Some(instance))
    }
}

pub fn acquire() -> Result<Option<Instance>> {
    acquire_with_activation(true)
}
pub fn acquire_background() -> Result<Option<Instance>> {
    acquire_with_activation(false)
}
fn acquire_with_activation(activate: bool) -> Result<Option<Instance>> {
    // The confirmed updater owns startup until replacement or recovery finishes.
    if acquire_named(UPDATING.pcwstr())?.is_none() {
        return Ok(None);
    }
    let result = acquire_named(CONTROL_CENTER.pcwstr());
    let message = match &result {
        Ok(Some(_)) => return result,
        Ok(None) => {
            if activate {
                activate_existing()?;
            }
            return result;
        }
        Err(error) => format!("无法启动 {}：{error:#}", product::display_name()),
    };
    let message: Vec<u16> = message.encode_utf16().chain(Some(0)).collect();
    let title = product::wide(&product::display_name());
    unsafe {
        MessageBoxW(
            None,
            PCWSTR(message.as_ptr()),
            PCWSTR(title.as_ptr()),
            MB_OK | MB_ICONERROR | MB_SETFOREGROUND,
        );
    }
    result
}

pub(crate) fn register_window(hwnd: HWND) -> Result<()> {
    anyhow::ensure!(show_message() != 0, "注册控制中心恢复消息失败");
    unsafe {
        SetWindowSubclass(hwnd, Some(window_message), SHOW_SUBCLASS, 0).ok()?;
    }
    let result = unsafe {
        SetPropW(
            hwnd,
            WINDOW_PROPERTY.pcwstr(),
            Some(HANDLE(hwnd.0)),
        )
    }
    .context("标记控制中心窗口失败");
    if result.is_err() {
        unsafe {
            let _ = RemoveWindowSubclass(hwnd, Some(window_message), SHOW_SUBCLASS);
        }
    }
    result
}

unsafe extern "system" fn window_message(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
    _id: usize,
    _data: usize,
) -> LRESULT {
    if message == windows::Win32::UI::WindowsAndMessaging::WM_COPYDATA && lparam.0 != 0 {
        // WM_COPYDATA owns this buffer for the duration of the synchronous call.
        let data =
            unsafe { &*(lparam.0 as *const windows::Win32::System::DataExchange::COPYDATASTRUCT) };
        if data.dwData == NOTIFICATION_MESSAGE
            && data.cbData <= 256
            && data.cbData > 0
            && !data.lpData.is_null()
        {
            let bytes = unsafe {
                std::slice::from_raw_parts(data.lpData.cast::<u8>(), data.cbData as usize)
            };
            if let Ok(uri) = std::str::from_utf8(bytes) {
                return LRESULT(isize::from(
                    crate::platform::windows::notifications::receive_activation(uri),
                ));
            }
        }
        return LRESULT(0);
    }
    if message == exit_message() {
        if crate::platform::windows::components::maintaining() {
            return LRESULT(2);
        }
        // Zero retains the update behavior of existing senders. Removal closes
        // sessions normally instead of asking remote viewers to wait for an update.
        UPDATE_EXIT.store(wparam.0 != 1, std::sync::atomic::Ordering::Release);
        return LRESULT(
            if crate::ui::window_manager::send(crate::ui::window_manager::Request::Exit).is_ok() {
                1
            } else {
                2
            },
        );
    }
    if message == show_message() {
        // Updating visibility through the UI owner keeps winit's WindowFlags in
        // sync. Showing the HWND directly leaves VISIBLE false after tray hide.
        let _ = crate::ui::window_manager::send(crate::ui::window_manager::Request::ShowMain);
        return LRESULT(0);
    }
    if message == WM_NCDESTROY {
        unsafe {
            let _ = RemovePropW(hwnd, WINDOW_PROPERTY.pcwstr());
            let _ = RemoveWindowSubclass(hwnd, Some(window_message), SHOW_SUBCLASS);
        }
    }
    unsafe { DefSubclassProc(hwnd, message, wparam, lparam) }
}
pub(crate) fn reserve_after_exit() -> Result<Option<Instance>> {
    acquire_named(CONTROL_CENTER.pcwstr())
}

fn activate_existing() -> Result<()> {
    unsafe extern "system" fn find(hwnd: HWND, data: LPARAM) -> BOOL {
        if !unsafe { GetPropW(hwnd, WINDOW_PROPERTY.pcwstr()) }.is_invalid() {
            unsafe {
                *(data.0 as *mut Option<HWND>) = Some(hwnd);
            }
            return false.into();
        }
        true.into()
    }
    let mut window: Option<HWND> = None;
    unsafe {
        let _ = EnumWindows(Some(find), LPARAM(&mut window as *mut _ as isize));
        if let Some(hwnd) = window {
            let mut pid = 0;
            GetWindowThreadProcessId(hwnd, Some(&mut pid));
            if pid != 0 {
                let _ = AllowSetForegroundWindow(pid);
            }
            let message = show_message();
            anyhow::ensure!(message != 0, "注册控制中心恢复消息失败");
            PostMessageW(Some(hwnd), message, WPARAM(0), LPARAM(0))?;
        }
    }
    Ok(())
}

fn exit_message() -> u32 {
    static MESSAGE: std::sync::OnceLock<u32> = std::sync::OnceLock::new();
    *MESSAGE.get_or_init(|| unsafe {
        RegisterWindowMessageW(EXIT_MESSAGE.pcwstr())
    })
}
pub(crate) fn reserve_installer() -> Result<Instance> {
    acquire_named(INSTALLER.pcwstr())?.context("已有安装或卸载窗口")
}
pub(crate) fn reserve_update() -> Result<Instance> {
    acquire_named(UPDATING.pcwstr())?.context("已有程序更新正在进行")
}
pub(crate) struct Running {
    window: HWND,
    process: crate::platform::windows::host_service::pipe::Handle,
}
pub(crate) fn running_installed() -> Result<Option<Running>> {
    running_image(&crate::platform::windows::components::application::image()?)
}
fn running_image(expected: &std::path::Path) -> Result<Option<Running>> {
    use crate::platform::windows::host_service::{pipe::Handle, process, vault};
    use windows::Win32::System::Threading::*;
    unsafe extern "system" fn collect(hwnd: HWND, data: LPARAM) -> BOOL {
        let candidates = unsafe { &mut *(data.0 as *mut Vec<HWND>) };
        if !unsafe { GetPropW(hwnd, WINDOW_PROPERTY.pcwstr()) }.is_invalid() {
            candidates.push(hwnd);
        }
        true.into()
    }
    let mut candidates: Vec<HWND> = Vec::new();
    unsafe {
        EnumWindows(Some(collect), LPARAM(&mut candidates as *mut _ as isize))?;
    }
    let image = std::fs::canonicalize(expected)?;
    let own = std::process::id();
    let sid = vault::sid(own)?;
    let session = process::session(own)?;
    let mut found = None;
    for window in candidates {
        let mut pid = 0;
        unsafe {
            GetWindowThreadProcessId(window, Some(&mut pid));
        }
        if pid == 0 || pid == own {
            continue;
        }
        let Ok(handle) = (unsafe {
            OpenProcess(
                PROCESS_SYNCHRONIZE | PROCESS_QUERY_LIMITED_INFORMATION,
                false,
                pid,
            )
        }) else {
            continue;
        };
        let pinned = Handle(handle);
        let Ok(path) = process::image(pid).and_then(|p| Ok(std::fs::canonicalize(p)?)) else {
            continue;
        };
        if process::session(pid)? != session || vault::sid(pid)? != sid {
            continue;
        }
        let same_image = path
            .as_os_str()
            .to_string_lossy()
            .eq_ignore_ascii_case(&image.as_os_str().to_string_lossy());
        if !same_image {
            continue;
        }
        anyhow::ensure!(
            process::session(pid)? == session && vault::sid(pid)? == sid,
            "运行版本不属于当前 Windows 用户会话"
        );
        anyhow::ensure!(found.is_none(), "检测到多个控制中心");
        // Pin the process handle and verify the HWND still belongs to it.
        let mut current = 0;
        unsafe {
            GetWindowThreadProcessId(window, Some(&mut current));
        }
        anyhow::ensure!(current == pid, "运行程序已变化");
        found = Some(Running {
            window,
            process: pinned,
        });
    }
    Ok(found)
}
pub(crate) fn deliver_notification(uri: &str) -> Result<()> {
    use windows::Win32::{System::DataExchange::COPYDATASTRUCT, UI::WindowsAndMessaging::*};
    anyhow::ensure!(uri.len() <= 256, "通知参数过长");
    let running =
        running_image(&std::env::current_exe()?)?.context("通知已过期或控制中心已退出")?;
    // Protocol activation carries the user's foreground grant to the existing UI.
    let mut pid = 0;
    unsafe {
        GetWindowThreadProcessId(running.window, Some(&mut pid));
    }
    if pid != 0 {
        let _ = unsafe { AllowSetForegroundWindow(pid) };
    }
    let data = COPYDATASTRUCT {
        dwData: NOTIFICATION_MESSAGE,
        cbData: uri.len() as u32,
        lpData: uri.as_ptr().cast_mut().cast(),
    };
    let mut accepted = 0usize;
    let result = unsafe {
        SendMessageTimeoutW(
            running.window,
            WM_COPYDATA,
            WPARAM(0),
            LPARAM(&data as *const _ as isize),
            SMTO_ABORTIFHUNG,
            2000,
            Some(&mut accepted),
        )
    };
    anyhow::ensure!(
        result.0 != 0 && accepted == 1,
        "通知已失效或控制中心暂时不可用"
    );
    Ok(())
}
impl Running {
    pub fn close(self, updating: bool) -> Result<()> {
        use windows::Win32::{
            Foundation::WAIT_OBJECT_0, System::Threading::*, UI::WindowsAndMessaging::*,
        };
        if unsafe { WaitForSingleObject(self.process.0, 0) } == WAIT_OBJECT_0 {
            return Ok(());
        }
        if !unsafe { IsWindow(Some(self.window)) }.as_bool() {
            anyhow::ensure!(
                unsafe { WaitForSingleObject(self.process.0, 45_000) } == WAIT_OBJECT_0,
                "等待旧版本退出超时"
            );
            return Ok(());
        }
        let mut reply = 0usize;
        let message = exit_message();
        anyhow::ensure!(message != 0, "注册更新退出消息失败");
        anyhow::ensure!(
            unsafe {
                SendMessageTimeoutW(
                    self.window,
                    message,
                    WPARAM(usize::from(!updating)),
                    LPARAM(0),
                    SMTO_ABORTIFHUNG | SMTO_BLOCK,
                    2000,
                    Some(&mut reply),
                )
            }
            .0 != 0,
            "旧版本未响应退出请求"
        );
        anyhow::ensure!(reply != 2, "旧版本正在处理组件操作");
        if reply == 0 {
            // Existing builds predate ExitForUpdate. Their current winit owner
            // accepts this registered destroy request and runs the application's
            // on_exit/shutdown cleanup. Never synthesize WM_DESTROY or kill a PID.
            let destroy = unsafe { RegisterWindowMessageW(w!("Winit::DestroyMsg")) };
            anyhow::ensure!(destroy != 0, "注册窗口退出消息失败");
            unsafe {
                PostMessageW(Some(self.window), destroy, WPARAM(0), LPARAM(0))?;
            }
        }
        anyhow::ensure!(
            unsafe { WaitForSingleObject(self.process.0, 45_000) } == WAIT_OBJECT_0,
            "等待旧版本退出超时"
        );
        Ok(())
    }
}
