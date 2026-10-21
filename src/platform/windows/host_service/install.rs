//! Deployment and identity of the optional privileged host service.
use crate::i18n::t;
use crate::platform::windows::components::{Status, application as deployment};
use anyhow::{Context, Result, ensure};
use std::{
    mem::size_of,
    path::PathBuf,
    time::{Duration, Instant},
};
use windows::{
    Win32::{
        Foundation::*, Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT, System::Services::*,
    },
    core::PCWSTR,
};
const SERVICE: &str = crate::product::SERVICE_NAME;
use deployment::RECEIPT;
#[derive(Debug, thiserror::Error)]
#[error("{}", crate::i18n::t().system.service_update_required)]
pub(crate) struct NeedsUpdate;
fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}
struct Sc(SC_HANDLE);
impl Drop for Sc {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseServiceHandle(self.0);
        }
    }
}
pub(crate) fn directory() -> Result<PathBuf> {
    deployment::directory()
}
fn command() -> Result<String> {
    Ok(format!("\"{}\" service", deployment::image()?.display()))
}
#[derive(serde::Serialize, serde::Deserialize)]
struct Images {
    client: String,
    host: String,
}
fn images() -> Result<Images> {
    let path = directory()?.join("images.json");
    ensure!(
        std::fs::metadata(&path)?.len() < 4096,
        "{}",
        t().system.service_invalid_registration
    );
    Ok(serde_json::from_slice(&std::fs::read(path)?)?)
}
pub(crate) fn verify_client(pid: u32) -> Result<()> {
    ensure!(
        super::process::image_hash(&super::process::image(pid)?)? == images()?.client,
        "{}",
        t().system.service_version_mismatch
    );
    Ok(())
}
fn manager(access: u32) -> Result<Sc> {
    Ok(Sc(unsafe { OpenSCManagerW(None, None, access)? }))
}
fn service(manager: &Sc, access: u32) -> Result<Option<Sc>> {
    match unsafe { OpenServiceW(manager.0, PCWSTR(wide(SERVICE).as_ptr()), access) } {
        Ok(h) => Ok(Some(Sc(h))),
        Err(e) if e.code() == ERROR_SERVICE_DOES_NOT_EXIST.to_hresult() => Ok(None),
        Err(e) => Err(e.into()),
    }
}
fn verify_service(service: &Sc) -> Result<bool> {
    // An aligned buffer avoids interpreting a Vec<u8> as a pointer-aligned struct.
    let mut buffer = vec![0usize; 1024];
    let mut needed = 0;
    unsafe {
        QueryServiceConfigW(
            service.0,
            Some(buffer.as_mut_ptr().cast()),
            (buffer.len() * size_of::<usize>()) as u32,
            &mut needed,
        )?;
        let config = &*buffer.as_ptr().cast::<QUERY_SERVICE_CONFIGW>();
        let path = config.lpBinaryPathName.to_string()?;
        let current = path.eq_ignore_ascii_case(&command()?);
        ensure!(
            config.dwServiceType == SERVICE_WIN32_OWN_PROCESS && current,
            "{}",
            t().system.service_not_owned
        );
        Ok(current)
    }
}
pub(crate) fn status() -> Result<Status> {
    let manager = manager(SC_MANAGER_CONNECT)?;
    let Some(service) = service(&manager, SERVICE_QUERY_STATUS | SERVICE_QUERY_CONFIG)? else {
        let dir = directory()?;
        let owned = dir.join("owner.txt").is_file();
        if owned {
            ensure!(
                std::fs::read_to_string(dir.join("owner.txt"))? == RECEIPT,
                "{}",
                t().system.service_directory_not_owned
            );
        }
        return Ok(Status {
            label: t().system.install_not_installed.into(),
            installed: false,
            removable: owned,
            ready: false,
        });
    };
    let current = verify_service(&service)?;
    let compatible = images()
        .and_then(|images| {
            Ok(
                super::process::image_hash(&std::env::current_exe()?)? == images.client
                    && super::process::image_hash(&deployment::image()?)? == images.host,
            )
        })
        .unwrap_or(false);
    let mut state = SERVICE_STATUS::default();
    unsafe {
        QueryServiceStatus(service.0, &mut state)?;
    }
    Ok(Status {
        label: if !current || !compatible {
            t().system.install_needs_update
        } else if state.dwCurrentState == SERVICE_RUNNING {
            t().system.service_running
        } else {
            t().system.service_stopped
        }
        .into(),
        installed: true,
        removable: true,
        ready: current && compatible && state.dwCurrentState == SERVICE_RUNNING,
    })
}
pub(crate) fn verify_running(pid: u32) -> Result<()> {
    verify_endpoint(pid, true)
}
pub(crate) fn verify_resident(pid: u32) -> Result<()> {
    verify_endpoint(pid, false)
}
fn verify_endpoint(pid: u32, check_client: bool) -> Result<()> {
    let manager = manager(SC_MANAGER_CONNECT)?;
    let service = service(&manager, SERVICE_QUERY_CONFIG | SERVICE_QUERY_STATUS)?
        .context(t().system.service_host_not_installed)?;
    if !verify_service(&service)? {
        return Err(NeedsUpdate.into());
    }
    let mut state = SERVICE_STATUS_PROCESS::default();
    let mut needed = 0;
    let bytes = unsafe {
        std::slice::from_raw_parts_mut(
            (&mut state as *mut SERVICE_STATUS_PROCESS).cast(),
            size_of::<SERVICE_STATUS_PROCESS>(),
        )
    };
    unsafe {
        QueryServiceStatusEx(service.0, SC_STATUS_PROCESS_INFO, Some(bytes), &mut needed)?;
    }
    ensure!(
        pid != 0 && state.dwCurrentState == SERVICE_RUNNING && state.dwProcessId == pid,
        "{}",
        t().system.service_invalid_identity
    );
    // Ordinary users cannot open the SYSTEM process. SCM owns both the verified
    // executable path and running PID; the pipe supplies its server PID.
    let registered = images()?;
    if check_client {
        let own = super::process::image_hash(&std::env::current_exe()?)?;
        if own != registered.client && own != registered.host {
            return Err(NeedsUpdate.into());
        }
    }
    ensure!(
        super::process::image_hash(&deployment::image()?)? == registered.host,
        "{}",
        t().system.service_files_mismatch
    );
    Ok(())
}
fn stop(service: &Sc) -> Result<()> {
    let mut state = SERVICE_STATUS::default();
    unsafe {
        QueryServiceStatus(service.0, &mut state)?;
    }
    if state.dwCurrentState == SERVICE_STOPPED {
        return Ok(());
    }
    unsafe {
        ControlService(service.0, SERVICE_CONTROL_STOP, &mut state)?;
    }
    let start = Instant::now();
    loop {
        unsafe {
            QueryServiceStatus(service.0, &mut state)?;
        }
        if state.dwCurrentState == SERVICE_STOPPED {
            return Ok(());
        }
        ensure!(
            start.elapsed() < Duration::from_secs(10),
            "{}",
            t().system.service_stop_timeout
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[derive(Debug, thiserror::Error)]
#[error("{}", crate::i18n::t().system.service_active_sessions)]
pub(crate) struct ActiveSession;

// A repair/uninstall cannot invalidate a live input or capture agent.
fn ensure_idle(service: &Sc) -> Result<()> {
    use windows::Win32::System::Diagnostics::ToolHelp::*;
    let mut state = SERVICE_STATUS_PROCESS::default();
    let mut needed = 0;
    let bytes = unsafe {
        std::slice::from_raw_parts_mut(
            (&mut state as *mut SERVICE_STATUS_PROCESS).cast(),
            size_of::<SERVICE_STATUS_PROCESS>(),
        )
    };
    unsafe {
        QueryServiceStatusEx(service.0, SC_STATUS_PROCESS_INFO, Some(bytes), &mut needed)?;
    }
    if state.dwProcessId == 0 {
        return Ok(());
    }
    let resident = std::fs::read_to_string(directory()?.join("resident.pid"))
        .ok()
        .and_then(|v| v.parse::<u32>().ok())
        .unwrap_or(0);
    if resident != 0 {
        ensure!(
            matches!(
                super::resident::call(super::resident::Request::Quiescent)?.checked()?,
                super::resident::Reply::Done
            ),
            "{}",
            t().system.service_not_drained
        );
    }
    let snapshot = super::pipe::Handle(unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0)? });
    let mut entry = PROCESSENTRY32W {
        dwSize: size_of::<PROCESSENTRY32W>() as u32,
        ..Default::default()
    };
    let mut result = unsafe { Process32FirstW(snapshot.0, &mut entry) };
    while result.is_ok() {
        let len = entry
            .szExeFile
            .iter()
            .position(|c| *c == 0)
            .unwrap_or(entry.szExeFile.len());
        if !(entry.th32ProcessID == resident
            || entry.th32ParentProcessID != state.dwProcessId
            || !String::from_utf16_lossy(&entry.szExeFile[..len]).eq_ignore_ascii_case(
                deployment::image()?
                    .file_name()
                    .context(t().system.component_invalid_install_file_name)?
                    .to_str()
                    .context(t().system.component_invalid_install_file_name)?,
            ))
        {
            // A service-launched ordinary control center is not a privileged
            // capture/input lease. Its host work was drained above; require the
            // registered application image before excluding it from this check.
            if super::vault::sid(entry.th32ProcessID)? != "S-1-5-18" {
                verify_client(entry.th32ProcessID)?;
            } else {
                return Err(ActiveSession.into());
            }
        }
        result = unsafe { Process32NextW(snapshot.0, &mut entry) };
    }
    if let Err(e) = result {
        ensure!(
            e.code() == ERROR_NO_MORE_FILES.to_hresult(),
            "{}",
            (t().system.service_usage_check_failed)(&e.to_string())
        );
    }
    Ok(())
}
pub(crate) fn running() -> Result<bool> {
    let manager = manager(SC_MANAGER_CONNECT)?;
    let Some(service) = service(&manager, SERVICE_QUERY_CONFIG | SERVICE_QUERY_STATUS)? else {
        return Ok(false);
    };
    verify_service(&service)?;
    let mut state = SERVICE_STATUS::default();
    unsafe {
        QueryServiceStatus(service.0, &mut state)?;
    }
    Ok(state.dwCurrentState != SERVICE_STOPPED)
}
pub(crate) fn preflight() -> Result<()> {
    let manager = manager(SC_MANAGER_CONNECT)?;
    if let Some(service) = service(&manager, SERVICE_QUERY_CONFIG | SERVICE_QUERY_STATUS)? {
        verify_service(&service)?;
        ensure_idle(&service)?;
    }
    Ok(())
}
pub(crate) fn stop_for_maintenance() -> Result<()> {
    let manager = manager(SC_MANAGER_CONNECT)?;
    if let Some(service) = service(
        &manager,
        SERVICE_QUERY_CONFIG | SERVICE_QUERY_STATUS | SERVICE_STOP,
    )? {
        verify_service(&service)?;
        ensure_idle(&service)?;
        stop(&service)?;
    }
    Ok(())
}
pub(crate) fn start_installed() -> Result<()> {
    let manager = manager(SC_MANAGER_CONNECT)?;
    let service = service(
        &manager,
        SERVICE_QUERY_CONFIG | SERVICE_QUERY_STATUS | SERVICE_START,
    )?
    .context(t().system.service_host_not_installed)?;
    verify_service(&service)?;
    let mut state = SERVICE_STATUS::default();
    unsafe {
        QueryServiceStatus(service.0, &mut state)?;
        if state.dwCurrentState != SERVICE_RUNNING {
            StartServiceW(service.0, None)?;
        }
    }
    Ok(())
}
pub(crate) fn install(allow_sas: bool) -> Result<bool> {
    let registered = Images {
        client: super::process::image_hash(&std::env::current_exe()?)?,
        host: super::process::image_hash(&std::env::current_exe()?)?,
    };
    let manager = manager(SC_MANAGER_CONNECT | SC_MANAGER_CREATE_SERVICE)?;
    let existing = service(
        &manager,
        SERVICE_QUERY_CONFIG
            | SERVICE_QUERY_STATUS
            | SERVICE_STOP
            | SERVICE_START
            | SERVICE_CHANGE_CONFIG,
    )?;
    if let Some(service) = &existing {
        verify_service(service)?;
        ensure_idle(service)?;
    }
    let previous_command = command()?;
    let had_sas_policy = directory()?.join("sas-policy.json").exists();
    let was_running = if let Some(service) = &existing {
        let mut state = SERVICE_STATUS::default();
        unsafe {
            QueryServiceStatus(service.0, &mut state)?;
        }
        stop(service)?;
        state.dwCurrentState == SERVICE_RUNNING
    } else {
        false
    };
    let transaction = match deployment::Deployment::prepare() {
        Ok(value) => value,
        Err(error) => {
            if was_running {
                if let Some(service) = &existing {
                    let _ = unsafe { StartServiceW(service.0, None) };
                }
            }
            return Err(error);
        }
    };
    let result = (|| -> Result<bool> {
        let directory = directory()?;
        std::fs::write(
            directory.join("images.json"),
            serde_json::to_vec(&registered)?,
        )?;
        if allow_sas {
            super::sas_policy::enable(&directory)?;
        }
        let service = match &existing {
            Some(s) => {
                unsafe {
                    ChangeServiceConfigW(
                        s.0,
                        ENUM_SERVICE_TYPE(SERVICE_NO_CHANGE),
                        SERVICE_AUTO_START,
                        SERVICE_ERROR(SERVICE_NO_CHANGE),
                        PCWSTR(wide(&command()?).as_ptr()),
                        None,
                        None,
                        None,
                        None,
                        None,
                        PCWSTR(wide(crate::product::SERVICE_DISPLAY_NAME).as_ptr()),
                    )?;
                }
                None
            }
            None => Some(Sc(unsafe {
                CreateServiceW(
                    manager.0,
                    PCWSTR(wide(SERVICE).as_ptr()),
                    PCWSTR(wide(crate::product::SERVICE_DISPLAY_NAME).as_ptr()),
                    SERVICE_QUERY_CONFIG
                        | SERVICE_QUERY_STATUS
                        | SERVICE_START
                        | SERVICE_STOP
                        | SERVICE_CHANGE_CONFIG,
                    SERVICE_WIN32_OWN_PROCESS,
                    SERVICE_AUTO_START,
                    SERVICE_ERROR_NORMAL,
                    PCWSTR(wide(&command()?).as_ptr()),
                    None,
                    None,
                    None,
                    None,
                    None,
                )?
            })),
        };
        let service = service
            .as_ref()
            .or(existing.as_ref())
            .context(t().system.service_handle_missing)?;
        let mut actions = [
            SC_ACTION {
                Type: SC_ACTION_RESTART,
                Delay: 1000,
            },
            SC_ACTION {
                Type: SC_ACTION_RESTART,
                Delay: 2000,
            },
            SC_ACTION {
                Type: SC_ACTION_RESTART,
                Delay: 4000,
            },
            SC_ACTION {
                Type: SC_ACTION_NONE,
                Delay: 0,
            },
        ];
        let recovery = SERVICE_FAILURE_ACTIONSW {
            dwResetPeriod: 900,
            cActions: actions.len() as u32,
            lpsaActions: actions.as_mut_ptr(),
            ..Default::default()
        };
        unsafe {
            ChangeServiceConfig2W(
                service.0,
                SERVICE_CONFIG_FAILURE_ACTIONS,
                Some((&recovery as *const SERVICE_FAILURE_ACTIONSW).cast()),
            )?;
        }
        deployment::register()?;
        Ok(false)
    })();
    match result {
        Ok(value) => {
            transaction.commit();
            Ok(value)
        }
        Err(error) => {
            if let Ok(Some(failed)) =
                service(&manager, SERVICE_STOP | SERVICE_QUERY_STATUS | 0x10000)
            {
                let _ = stop(&failed);
                if existing.is_none() {
                    let _ = unsafe { DeleteService(failed.0) };
                }
            }
            if allow_sas && !had_sas_policy {
                let _ = super::sas_policy::restore(&directory()?);
            }
            drop(transaction);
            if let Some(service) = &existing {
                unsafe {
                    let _ = ChangeServiceConfigW(
                        service.0,
                        ENUM_SERVICE_TYPE(SERVICE_NO_CHANGE),
                        SERVICE_START_TYPE(SERVICE_NO_CHANGE),
                        SERVICE_ERROR(SERVICE_NO_CHANGE),
                        PCWSTR(wide(&previous_command).as_ptr()),
                        None,
                        None,
                        None,
                        None,
                        None,
                        None,
                    );
                    if was_running {
                        let _ = StartServiceW(service.0, None);
                    }
                }
            }
            Err(error)
        }
    }
}

pub(crate) fn uninstall() -> Result<bool> {
    let directory = directory()?;
    if directory.exists() {
        use std::os::windows::fs::MetadataExt;
        ensure!(
            std::fs::symlink_metadata(&directory)?.file_attributes()
                & FILE_ATTRIBUTE_REPARSE_POINT.0
                == 0,
            "{}",
            t().system.service_directory_reparse
        );
        ensure!(
            std::fs::read_to_string(directory.join("owner.txt"))? == RECEIPT,
            "{}",
            t().system.service_directory_not_owned_kept
        );
    }

    let manager = manager(SC_MANAGER_CONNECT)?;
    let service = service(
        &manager,
        SERVICE_QUERY_CONFIG | SERVICE_QUERY_STATUS | SERVICE_STOP | 0x10000,
    )?;
    if let Some(service) = &service {
        verify_service(service)?;
        ensure_idle(service)?;
        stop(service)?;
    }
    if let Some(service) = service {
        unsafe {
            DeleteService(service.0)?;
        }
    }
    // One image now serves both the ordinary application and privileged roles.
    // Uninstall the service, not the running application. In particular, never
    // queue deletion of its stable path: a later reinstall could use that path.
    for name in ["images.json", "resident.pid", "display-agent.json"] {
        let file = directory.join(name);
        if file.exists() {
            std::fs::remove_file(file)?;
        }
    }
    if directory.exists() {
        super::sas_policy::restore(&directory)?;
    }
    Ok(false)
}
