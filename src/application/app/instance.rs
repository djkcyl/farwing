//! One control center per desktop login session, independent of the EXE name.

/// 第二个进程要不要把已经在跑的控制中心叫到前台，以及顺带交给它的东西。
#[derive(Clone, Copy)]
pub enum Activation<'a> {
    /// 再点一次图标：把窗口叫出来就行。
    Show,
    /// 系统按 `uuremote://` 协议注册拉起的协助链接。链接原样转发，由接手的
    /// 那个进程解析——窗口消息只负责搬运。
    Link(&'a str),
    /// 后台启动，不打扰已经在跑的实例。
    Quiet,
}

#[cfg(windows)]
mod platform {
    use anyhow::{Context, Result};
    use windows::Win32::Foundation::{
        CloseHandle, ERROR_ALREADY_EXISTS, GetLastError, HANDLE, HWND, LPARAM, LRESULT, WPARAM,
    };
    use windows::Win32::System::Threading::CreateMutexW;
    use windows::Win32::UI::Shell::{DefSubclassProc, RemoveWindowSubclass, SetWindowSubclass};
    use windows::Win32::UI::WindowsAndMessaging::{
        AllowSetForegroundWindow, EnumWindows, GetPropW, GetWindowThreadProcessId, MB_ICONERROR,
        MB_OK, MB_SETFOREGROUND, MessageBoxW, PostMessageW, RegisterWindowMessageW, RemovePropW,
        SetPropW, WM_NCDESTROY,
    };
    use windows::core::{BOOL, PCWSTR, w};

    use super::Activation;

    pub struct Instance(HANDLE);
    const SHOW_SUBCLASS: usize = 0x4f554943;
    /// 激活链接在 WM_COPYDATA 上的标记；通知按钮和协助链接共用一条通道，
    /// 由 URI 的 scheme 区分。
    const ACTIVATION_MESSAGE: usize = 0x4f554e54;
    /// 一条激活链接的上限：协助链接要带上设备 ID、验证码和几个来源参数。
    const MAX_ACTIVATION: usize = 1024;
    const DELIVER_ATTEMPTS: usize = 5;
    const DELIVER_INTERVAL: std::time::Duration = std::time::Duration::from_millis(200);
    static UPDATE_EXIT: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    pub(crate) fn take_update_exit() -> bool {
        UPDATE_EXIT.swap(false, std::sync::atomic::Ordering::AcqRel)
    }

    fn show_message() -> u32 {
        static MESSAGE: std::sync::OnceLock<u32> = std::sync::OnceLock::new();
        *MESSAGE
            .get_or_init(|| unsafe { RegisterWindowMessageW(w!("OpenUUYC.ControlCenter.Show.v1")) })
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

    /// 抢实例锁。抢不到就把这次启动的来意交给已经在跑的那个实例，然后自己去退出。
    pub fn acquire(activation: Activation<'_>) -> Result<Option<Instance>> {
        // The confirmed updater owns startup until replacement or recovery finishes.
        if acquire_named(w!("Local\\OpenUUYC.Updating.v1"))?.is_none() {
            return Ok(None);
        }
        let result = acquire_named(w!("Local\\OpenUUYC.ControlCenter.v1"));
        let message = match &result {
            Ok(Some(_)) => return result,
            Ok(None) => {
                match activation {
                    // A second start and a protocol activation both hand the
                    // foreground grant to the window that already owns the UI.
                    Activation::Quiet => (),
                    Activation::Show => activate_existing()?,
                    Activation::Link(uri) => deliver_uri(uri)?,
                }
                return result;
            }
            Err(error) => format!("无法启动 OpenUUYC：{error:#}"),
        };
        let message: Vec<u16> = message.encode_utf16().chain(Some(0)).collect();
        unsafe {
            MessageBoxW(
                None,
                PCWSTR(message.as_ptr()),
                w!("OpenUUYC"),
                MB_OK | MB_ICONERROR | MB_SETFOREGROUND,
            );
        }
        result
    }

    pub(crate) fn register_window(window: &winit::window::Window) -> Result<()> {
        use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};
        let RawWindowHandle::Win32(handle) = window.window_handle()?.as_raw() else {
            anyhow::bail!("控制中心窗口不是 Win32 窗口");
        };
        let hwnd = HWND(handle.hwnd.get() as *mut std::ffi::c_void);
        anyhow::ensure!(show_message() != 0, "注册控制中心恢复消息失败");
        unsafe {
            SetWindowSubclass(hwnd, Some(window_message), SHOW_SUBCLASS, 0).ok()?;
        }
        let result = unsafe {
            SetPropW(
                hwnd,
                w!("OpenUUYC.ControlCenter.Window.v1"),
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

    /// 转发进来的激活链接。通知按钮和协助链接共用一条通道，按 scheme 分开；
    /// 协助链接还要把控制中心叫到前台——用户是点了一条链接，不是点了一个按钮。
    fn receive_uri(uri: &str) -> bool {
        if uri.starts_with(crate::application::app::notifications::SCHEME) {
            return crate::platform::windows::notifications::receive_activation(uri);
        }
        crate::application::app::assist_link::receive(uri);
        // 回 1 让发送端停止重试；用户点的是链接，不能只入队却不见窗口。
        let _ = crate::ui::window_manager::send(crate::ui::window_manager::Request::ShowMain);
        true
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
            let data = unsafe {
                &*(lparam.0 as *const windows::Win32::System::DataExchange::COPYDATASTRUCT)
            };
            if data.dwData == ACTIVATION_MESSAGE
                && data.cbData as usize <= MAX_ACTIVATION
                && data.cbData > 0
                && !data.lpData.is_null()
            {
                let bytes = unsafe {
                    std::slice::from_raw_parts(data.lpData.cast::<u8>(), data.cbData as usize)
                };
                if let Ok(uri) = std::str::from_utf8(bytes) {
                    return LRESULT(isize::from(receive_uri(uri)));
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
                if crate::ui::window_manager::send(crate::ui::window_manager::Request::Exit).is_ok()
                {
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
                let _ = RemovePropW(hwnd, w!("OpenUUYC.ControlCenter.Window.v1"));
                let _ = RemoveWindowSubclass(hwnd, Some(window_message), SHOW_SUBCLASS);
            }
        }
        unsafe { DefSubclassProc(hwnd, message, wparam, lparam) }
    }
    pub(crate) fn reserve_after_exit() -> Result<Option<Instance>> {
        acquire_named(w!("Local\\OpenUUYC.ControlCenter.v1"))
    }

    fn activate_existing() -> Result<()> {
        unsafe extern "system" fn find(hwnd: HWND, data: LPARAM) -> BOOL {
            if !unsafe { GetPropW(hwnd, w!("OpenUUYC.ControlCenter.Window.v1")) }.is_invalid() {
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
            RegisterWindowMessageW(w!("OpenUUYC.ControlCenter.ExitForUpdate.v1"))
        })
    }
    pub(crate) fn reserve_installer() -> Result<Instance> {
        acquire_named(w!("Local\\OpenUUYC.Installer.v1"))?
            .context("已有安装或卸载窗口，请先完成该操作")
    }
    pub(crate) fn reserve_update() -> Result<Instance> {
        acquire_named(w!("Local\\OpenUUYC.Updating.v1"))?.context("已有程序更新正在进行")
    }
    pub(crate) struct Running {
        window: HWND,
        process: crate::platform::windows::host_service::pipe::Handle,
    }
    pub(crate) fn running_installed() -> Result<Option<Running>> {
        running_image(
            &crate::platform::windows::components::application::image()?,
            true,
        )
    }
    fn running_image(expected: &std::path::Path, migrating: bool) -> Result<Option<Running>> {
        use crate::platform::windows::host_service::{pipe::Handle, process, vault};
        use windows::Win32::System::Threading::*;
        struct Candidates {
            windows: Vec<HWND>,
            migrating: bool,
        }
        unsafe extern "system" fn collect(hwnd: HWND, data: LPARAM) -> BOOL {
            let candidates = unsafe { &mut *(data.0 as *mut Candidates) };
            if !unsafe { GetPropW(hwnd, w!("OpenUUYC.ControlCenter.Window.v1")) }.is_invalid()
                || candidates.migrating
                    && crate::platform::windows::components::migration::legacy_center(hwnd)
            {
                candidates.windows.push(hwnd);
            }
            true.into()
        }
        let mut candidates = Candidates {
            windows: Vec::new(),
            migrating,
        };
        unsafe {
            EnumWindows(Some(collect), LPARAM(&mut candidates as *mut _ as isize))?;
        }
        let image = std::fs::canonicalize(expected)?;
        let own = std::process::id();
        let sid = vault::sid(own)?;
        let session = process::session(own)?;
        let mut found = None;
        for window in candidates.windows {
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
            if !same_image
                && !(migrating
                    && crate::platform::windows::components::migration::registered_client(&path)?)
            {
                continue;
            }
            anyhow::ensure!(
                process::session(pid)? == session && vault::sid(pid)? == sid,
                "运行版本不属于当前Windows用户会话"
            );
            anyhow::ensure!(found.is_none(), "检测到多个控制中心，暂不能进行更新");
            // Pin the process handle and verify the HWND still belongs to it.
            let mut current = 0;
            unsafe {
                GetWindowThreadProcessId(window, Some(&mut current));
            }
            anyhow::ensure!(current == pid, "运行程序已变化，请重试更新");
            found = Some(Running {
                window,
                process: pinned,
            });
        }
        Ok(found)
    }
    /// 把激活链接（通知按钮或协助链接）交给已经在跑的控制中心。
    ///
    /// 协助链接是用户点出来的，控制中心可能正好在启动，所以比通知多等几轮；
    /// 错过就再也没有第二次机会了。
    pub(crate) fn deliver_uri(uri: &str) -> Result<()> {
        use windows::Win32::{System::DataExchange::COPYDATASTRUCT, UI::WindowsAndMessaging::*};
        anyhow::ensure!(uri.len() <= MAX_ACTIVATION, "链接参数过长");
        let assistance = uri
            .get(..9)
            .is_some_and(|s| s.eq_ignore_ascii_case("uuremote:"));
        let image = std::env::current_exe()?;
        let attempts = if assistance { DELIVER_ATTEMPTS } else { 1 };
        let mut running = None;
        for attempt in 0..attempts {
            running = running_image(&image, false)?;
            if running.is_some() {
                break;
            }
            if attempt + 1 < attempts {
                std::thread::sleep(DELIVER_INTERVAL);
            }
        }
        let running = running.context("请求已过期或控制中心已退出")?;
        // Retry only discovery. Once WM_COPYDATA is sent, a timeout has an unknown
        // delivery result and must not replay notifications or connection requests.
        // Protocol activation carries the user's foreground grant to the existing UI.
        let mut pid = 0;
        unsafe {
            GetWindowThreadProcessId(running.window, Some(&mut pid));
        }
        if pid != 0 {
            let _ = unsafe { AllowSetForegroundWindow(pid) };
        }
        let data = COPYDATASTRUCT {
            dwData: ACTIVATION_MESSAGE,
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
            "请求已失效或控制中心暂时不可用"
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
                    "等待运行版本完成退出超时，更新未开始"
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
                "运行版本未响应退出请求，更新未开始"
            );
            anyhow::ensure!(reply != 2, "运行版本正在处理组件操作，暂不能更新");
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
                "等待运行版本完成退出超时，更新未开始"
            );
            Ok(())
        }
    }
}

#[cfg(not(windows))]
mod platform {
    use super::Activation;
    use anyhow::{Context, Result};
    use std::fs::{File, OpenOptions};
    use std::path::PathBuf;

    /// The advisory lock lives as long as this handle; the file itself stays behind.
    pub struct Instance(#[allow(dead_code)] File);

    fn lock_path() -> PathBuf {
        let base = std::env::var_os("XDG_RUNTIME_DIR")
            .map(PathBuf::from)
            .filter(|path| path.is_dir())
            .unwrap_or_else(std::env::temp_dir);
        // Per-user, so a shared /tmp fallback cannot collide across accounts.
        base.join(format!("openuuyc-control-center-{}.lock", unsafe {
            libc::getuid()
        }))
    }

    /// 抢实例锁。抢不到就把这次启动的来意交给已经在跑的那个实例，然后自己去退出。
    pub fn acquire(activation: Activation<'_>) -> Result<Option<Instance>> {
        let path = lock_path();
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .with_context(|| format!("创建程序实例保护失败：{}", path.display()))?;
        match file.try_lock() {
            Ok(()) => Ok(Some(Instance(file))),
            Err(std::fs::TryLockError::WouldBlock) => match activation {
                // 运行的实例可能是服务，也可能只是收在托盘里，先把它的窗口叫出来；
                // 叫不动（旧版本、通道不可用）就保持沉默，别假装又起了一份。
                Activation::Show if crate::ui::control::activate().is_ok() => Ok(None),
                Activation::Show | Activation::Quiet => {
                    eprintln!("OpenUUYC 已在运行，请勿重复启动。");
                    Ok(None)
                }
                // 链接是用户点出来的，交不出去就得说出来，不能悄悄丢掉。
                Activation::Link(uri) => {
                    crate::ui::control::deliver_link(uri)?;
                    Ok(None)
                }
            },
            Err(std::fs::TryLockError::Error(error)) => {
                eprintln!("无法启动 OpenUUYC：{error}");
                Err(anyhow::Error::new(error).context("锁定程序实例保护失败"))
            }
        }
    }

    /// X11 and Wayland have no portable window-property handshake; the lock file
    /// is the whole reservation and the running window is left untouched.
    pub(crate) fn register_window(_window: &winit::window::Window) -> Result<()> {
        Ok(())
    }

    /// Only the Windows updater asks a running control center to exit.
    pub(crate) fn take_update_exit() -> bool {
        false
    }
}

pub use platform::{Instance, acquire};
#[cfg(windows)]
pub(crate) use platform::{
    deliver_uri, reserve_after_exit, reserve_installer, reserve_update, running_installed,
};
pub(crate) use platform::{register_window, take_update_exit};
