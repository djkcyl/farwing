//! Process GPU scheduling shared by active host capture/encoding owners.
//! Sunshine defaults to REALTIME; a High compatibility lease takes precedence.
//! Device-relative 7 alone cannot outrank a focused game.
use std::sync::Mutex;
use windows::{
    Win32::{
        Foundation::*,
        Graphics::{Direct3D11::ID3D11Device, Dxgi::IDXGIDevice},
        Security::*,
        System::Threading::{GetCurrentProcess, OpenProcessToken},
    },
    core::Interface,
};

const HIGH: i32 = 4;
const REALTIME: i32 = 5;

#[link(name = "gdi32")]
unsafe extern "system" {
    fn D3DKMTGetProcessSchedulingPriorityClass(process: HANDLE, priority: *mut i32) -> i32;
    fn D3DKMTSetProcessSchedulingPriorityClass(process: HANDLE, priority: i32) -> i32;
}

#[derive(Default)]
struct State {
    users: usize,
    compatible_users: usize,
    original: Option<i32>,
    requested: Option<i32>,
    applied: Option<i32>,
}
static PROCESS: Mutex<State> = Mutex::new(State {
    users: 0,
    compatible_users: 0,
    original: None,
    requested: None,
    applied: None,
});

// Enable only a privilege already held by our current token, with no elevation.
// PROCESS serializes users so restoring this privilege cannot race another
// scheduling call. Other token privileges are left unchanged.
struct PriorityPrivilege {
    token: super::host_service::pipe::Handle,
    previous: TOKEN_PRIVILEGES,
}
impl PriorityPrivilege {
    fn enable() -> anyhow::Result<Self> {
        unsafe {
            let mut raw = HANDLE::default();
            OpenProcessToken(
                GetCurrentProcess(),
                TOKEN_QUERY | TOKEN_ADJUST_PRIVILEGES,
                &mut raw,
            )?;
            let token = super::host_service::pipe::Handle(raw);
            let mut luid = LUID::default();
            LookupPrivilegeValueW(None, SE_INC_BASE_PRIORITY_NAME, &mut luid)?;
            let wanted = TOKEN_PRIVILEGES {
                PrivilegeCount: 1,
                Privileges: [LUID_AND_ATTRIBUTES {
                    Luid: luid,
                    Attributes: SE_PRIVILEGE_ENABLED,
                }],
            };
            let mut previous = TOKEN_PRIVILEGES::default();
            let mut length = 0;
            SetLastError(ERROR_SUCCESS);
            AdjustTokenPrivileges(
                token.0,
                false,
                Some(&wanted),
                std::mem::size_of_val(&previous) as u32,
                Some(&mut previous),
                Some(&mut length),
            )?;
            anyhow::ensure!(
                GetLastError() != ERROR_NOT_ALL_ASSIGNED,
                "当前身份没有GPU实时调度权限"
            );
            Ok(Self { token, previous })
        }
    }
}
impl Drop for PriorityPrivilege {
    fn drop(&mut self) {
        if self.previous.PrivilegeCount != 0 {
            unsafe {
                SetLastError(ERROR_SUCCESS);
                if let Err(error) =
                    AdjustTokenPrivileges(self.token.0, false, Some(&self.previous), 0, None, None)
                {
                    tracing::warn!(%error, "host GPU scheduling privilege restoration failed");
                } else if GetLastError() == ERROR_NOT_ALL_ASSIGNED {
                    tracing::warn!("host GPU scheduling privilege restoration not assigned");
                }
            }
        }
    }
}

fn query() -> Result<i32, i32> {
    let mut priority = 0;
    let status =
        unsafe { D3DKMTGetProcessSchedulingPriorityClass(GetCurrentProcess(), &mut priority) };
    if status >= 0 {
        Ok(priority)
    } else {
        Err(status)
    }
}

pub(super) fn configure_device(device: &ID3D11Device) {
    let _state = super::lock(&PROCESS);
    let _privilege = PriorityPrivilege::enable().ok();
    let result = device.cast::<IDXGIDevice>().and_then(|device| unsafe {
        device.SetGPUThreadPriority(7)?;
        device.GetGPUThreadPriority()
    });
    match result {
        Ok(7) => tracing::debug!(priority = 7, "host D3D11 GPU scheduling configured"),
        Ok(actual) => tracing::warn!(
            requested = 7,
            actual,
            "host D3D11 GPU scheduling readback differs"
        ),
        Err(error) => tracing::warn!(%error, "host D3D11 GPU scheduling unavailable"),
    }
}

impl State {
    fn update(&mut self) {
        let requested = if self.compatible_users > 0 {
            HIGH
        } else {
            REALTIME
        };
        if self.requested == Some(requested) {
            return;
        }
        self.requested = Some(requested);
        let Some(original) = self.original else {
            return;
        };
        let privilege = PriorityPrivilege::enable();
        let mut applied = requested;
        let mut status =
            unsafe { D3DKMTSetProcessSchedulingPriorityClass(GetCurrentProcess(), applied) };
        if status < 0 && requested == REALTIME {
            tracing::warn!(status, privilege_error = ?privilege.as_ref().err(),
                "host realtime GPU scheduling unavailable; trying High");
            applied = HIGH;
            status =
                unsafe { D3DKMTSetProcessSchedulingPriorityClass(GetCurrentProcess(), applied) };
        }
        if status < 0 {
            tracing::warn!(
                status,
                requested,
                "host GPU process scheduling denied; retaining current priority"
            );
            return;
        }
        self.applied = Some(applied);
        match query() {
            Ok(actual) if actual == applied => tracing::info!(
                original,
                requested,
                priority = actual,
                compatible_users = self.compatible_users,
                "host GPU process scheduling acquired"
            ),
            Ok(actual) => {
                // Preserve the value the OS actually accepted for teardown,
                // even if a driver clamps the requested priority.
                self.applied = Some(actual);
                tracing::warn!(
                    requested,
                    applied,
                    actual,
                    "host GPU process scheduling readback differs"
                );
            }
            Err(status) => tracing::warn!(
                requested,
                applied,
                status,
                "host GPU process scheduling readback failed"
            ),
        }
    }

    fn restore(&mut self) {
        self.requested = None;
        if let (Some(original), Some(applied)) = (self.original, self.applied) {
            if original != applied {
                if let Ok(actual) = query() {
                    if actual != applied {
                        tracing::info!(
                            actual,
                            applied,
                            "host GPU scheduling changed externally; preserving it"
                        );
                        self.original = None;
                        self.applied = None;
                        return;
                    }
                }
                let _privilege = PriorityPrivilege::enable().ok();
                let status = unsafe {
                    D3DKMTSetProcessSchedulingPriorityClass(GetCurrentProcess(), original)
                };
                if status < 0 {
                    // Keep the baseline after a failed restore, instead of
                    // adopting our own boost as a later lease's original value.
                    tracing::warn!(
                        status,
                        original,
                        "host GPU process scheduling restoration failed"
                    );
                    return;
                }
                match query() {
                    Ok(actual) if actual == original => {
                        tracing::info!(priority = actual, "host GPU process scheduling restored")
                    }
                    Ok(actual) if actual == applied => {
                        tracing::warn!(
                            original,
                            actual,
                            "host GPU scheduling restoration was not applied"
                        );
                        return;
                    }
                    Ok(actual) => tracing::warn!(
                        original,
                        actual,
                        "host GPU scheduling changed during restoration"
                    ),
                    Err(status) => tracing::warn!(
                        original,
                        status,
                        "host GPU scheduling restoration readback unavailable"
                    ),
                }
            }
        }
        self.original = None;
        self.applied = None;
    }
}

pub(super) struct Lease {
    compatible: bool,
}
impl Lease {
    pub fn new() -> Self {
        Self::with_compatibility(false)
    }
    pub fn with_compatibility(compatible: bool) -> Self {
        let mut state = super::lock(&PROCESS);
        if state.users == 0 && state.original.is_none() {
            match query() {
                Ok(priority) => state.original = Some(priority),
                Err(status) => tracing::warn!(status, "host GPU process priority query failed"),
            }
        }
        state.users += 1;
        state.compatible_users += usize::from(compatible);
        state.update();
        Self { compatible }
    }
}
impl Drop for Lease {
    fn drop(&mut self) {
        let mut state = super::lock(&PROCESS);
        state.users -= 1;
        state.compatible_users -= usize::from(self.compatible);
        if state.users == 0 {
            state.restore();
        } else {
            state.update();
        }
    }
}
