//! Optional privileged host service; independent from either driver package.
pub(crate) mod install;
pub(crate) mod pipe;
pub(crate) mod process;
mod sas_policy;
pub(crate) mod service;

pub(crate) mod activity;
pub(crate) mod resident;
pub(crate) mod startup;
pub(crate) mod user_backend;
pub(crate) mod vault;

/// The name reserving a device's presence room for one process machine-wide.
/// This mutex is never acquired: its HANDLE only reserves the object name.
pub(crate) struct PresenceReservation(#[allow(dead_code)] pipe::Handle);
unsafe impl Send for PresenceReservation {}

/// Reserve the device's presence room, or `None` while another process (the
/// portable client during installation, or the service) still holds it.
pub(crate) fn reserve_presence(device: &str) -> anyhow::Result<Option<PresenceReservation>> {
    use windows::{
        Win32::{
            Foundation::{ERROR_ALREADY_EXISTS, GetLastError},
            System::Threading::CreateMutexW,
        },
        core::PCWSTR,
    };
    let name: Vec<u16> = format!("Global\\OpenUUYC.Presence.{device}")
        .encode_utf16()
        .chain(Some(0))
        .collect();
    let handle = PresenceReservation(pipe::Handle(unsafe {
        CreateMutexW(None, false, PCWSTR(name.as_ptr()))?
    }));
    Ok((unsafe { GetLastError() } != ERROR_ALREADY_EXISTS).then_some(handle))
}

/// Whether an install, update or uninstall is replacing the service now.
pub(crate) fn maintaining() -> bool {
    super::components::maintaining()
}
pub(crate) use super::components::files::reject_reparse;
