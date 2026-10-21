use crate::i18n::t;
use anyhow::Result;
use std::sync::atomic::{AtomicBool, Ordering};
use windows::{
    Win32::{Foundation::*, System::Services::*},
    core::{PWSTR, w},
};
static STOP: AtomicBool = AtomicBool::new(false);
pub(crate) fn secure_attention() -> Result<()> {
    use windows::Win32::System::{LibraryLoader::*, Registry::*};
    unsafe {
        let mut policy = 0u32;
        let mut size = 4;
        let result = RegGetValueW(
            HKEY_LOCAL_MACHINE,
            w!(r"SOFTWARE\Microsoft\Windows\CurrentVersion\Policies\System"),
            w!("SoftwareSASGeneration"),
            RRF_RT_REG_DWORD,
            None,
            Some((&mut policy as *mut u32).cast()),
            Some(&mut size),
        );
        anyhow::ensure!(
            result.is_ok() && matches!(policy, 1 | 3),
            "{}",
            t().system.service_sas_not_allowed
        );
        let library = LoadLibraryExW(w!("sas.dll"), None, LOAD_LIBRARY_SEARCH_SYSTEM32)?;
        let entry = GetProcAddress(library, windows::core::s!("SendSAS"));
        if let Some(entry) = entry {
            let send: unsafe extern "system" fn(windows::core::BOOL) = std::mem::transmute(entry);
            send(false.into());
        }
        let _ = FreeLibrary(library);
        anyhow::ensure!(entry.is_some(), "{}", t().system.service_sas_unsupported);
    }
    Ok(())
}
pub(crate) fn run() -> Result<()> {
    let table = [
        SERVICE_TABLE_ENTRYW {
            lpServiceName: PWSTR(w!("FarwingHostService").as_ptr().cast_mut()),
            lpServiceProc: Some(main),
        },
        SERVICE_TABLE_ENTRYW::default(),
    ];
    unsafe {
        StartServiceCtrlDispatcherW(table.as_ptr())?;
    }
    Ok(())
}
unsafe extern "system" fn control(
    code: u32,
    _event: u32,
    _data: *mut core::ffi::c_void,
    _context: *mut core::ffi::c_void,
) -> u32 {
    match code {
        SERVICE_CONTROL_STOP | SERVICE_CONTROL_SHUTDOWN => {
            STOP.store(true, Ordering::Release);
            NO_ERROR.0
        }
        SERVICE_CONTROL_INTERROGATE => NO_ERROR.0,
        _ => ERROR_CALL_NOT_IMPLEMENTED.0,
    }
}
unsafe extern "system" fn main(_argc: u32, _argv: *mut PWSTR) {
    let result = (|| -> Result<()> {
        let handle = unsafe {
            RegisterServiceCtrlHandlerExW(w!("FarwingHostService"), Some(control), None)?
        };
        let mut state = SERVICE_STATUS {
            dwServiceType: SERVICE_WIN32_OWN_PROCESS,
            dwCurrentState: SERVICE_RUNNING,
            dwControlsAccepted: SERVICE_ACCEPT_STOP | SERVICE_ACCEPT_SHUTDOWN,
            ..Default::default()
        };
        unsafe {
            SetServiceStatus(handle, &state)?;
        }
        let result = serve();
        state.dwCurrentState = SERVICE_STOPPED;
        state.dwControlsAccepted = 0;
        state.dwWin32ExitCode = if result.is_ok() {
            0
        } else {
            ERROR_SERVICE_SPECIFIC_ERROR.0
        };
        state.dwServiceSpecificExitCode = if result.is_ok() { 0 } else { 1 };
        unsafe {
            SetServiceStatus(handle, &state)?;
        }
        result
    })();
    if let Err(error) = result {
        tracing::error!(%error,"input service stopped");
    }
}
fn serve() -> Result<()> {
    std::thread::scope(|scope| {
        let users =
            scope.spawn(|| super::user_backend::supervise(|| !STOP.load(Ordering::Acquire)));
        let residents = scope.spawn(|| {
            let result = super::resident::supervise(|| !STOP.load(Ordering::Acquire));
            if result.is_err() {
                STOP.store(true, Ordering::Release);
            }
            result
        });
        let captures = scope.spawn(|| {
            let result = super::super::capture_service::serve(|| !STOP.load(Ordering::Acquire));
            if result.is_err() {
                STOP.store(true, Ordering::Release);
            }
            result
        });
        let result = serve_input();
        STOP.store(true, Ordering::Release);
        let capture_result = captures
            .join()
            .map_err(|_| anyhow::anyhow!("采集服务线程异常"))?;
        let resident_result = residents
            .join()
            .map_err(|_| anyhow::anyhow!("后台服务线程异常"))?;
        result
            .and(
                users
                    .join()
                    .map_err(|_| anyhow::anyhow!("用户后台守护线程异常"))?,
            )
            .and(capture_result)
            .and(resident_result)
    })
}
fn serve_input() -> Result<()> {
    while !STOP.load(Ordering::Acquire) {
        let pipe = super::pipe::Pipe::server(&super::pipe::NAME, true)?;
        if pipe.accept(|| !STOP.load(Ordering::Acquire)).is_err() {
            continue;
        }
        if let Err(error) =
            crate::features::host::input::broker::serve(pipe, || !STOP.load(Ordering::Acquire))
        {
            tracing::warn!(%error,"input service connection ended");
        }
    }
    Ok(())
}
