//! Application ownership and module boundaries.

pub mod app;
pub mod cli;
pub(crate) mod dev_input;
pub(crate) mod dev_plugins;
pub(crate) mod dev_video;
pub mod bootstrap;
pub mod viewer;
pub(crate) mod viewer_shortcuts;
pub(crate) mod wallpaper;
static INSTALLED_HANDOFF: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
pub(crate) fn schedule_installed_handoff() {
    INSTALLED_HANDOFF.store(true, std::sync::atomic::Ordering::Release);
}
pub fn take_installed_handoff() -> bool {
    INSTALLED_HANDOFF.swap(false, std::sync::atomic::Ordering::AcqRel)
}
pub fn launch_installed(
    arguments: impl IntoIterator<Item = std::ffi::OsString>,
) -> anyhow::Result<()> {
    crate::platform::windows::components::application::start_installed(arguments)
}
pub fn route_installed_gui() -> anyhow::Result<bool> {
    app::maintenance::route_gui().inspect_err(app::maintenance::report_error)
}
pub fn uninstall_application(parent: Option<u32>) -> anyhow::Result<()> {
    app::maintenance::uninstall(parent).inspect_err(app::maintenance::report_error)
}
pub fn update_application(silent: bool, no_elevate: bool) -> anyhow::Result<bool> {
    app::maintenance::update(silent, no_elevate)
}
pub fn update_error_code(error: &anyhow::Error) -> i32 {
    app::maintenance::update_error_code(error)
}

pub fn host_service() -> anyhow::Result<()> {
    crate::platform::windows::host_service::service::run()
}
pub fn configure_wol(interface: String, mac: String) -> anyhow::Result<()> {
    crate::platform::windows::wol::setup::configure_elevated(interface, mac)
}
pub fn host_resident(parent: u32) -> anyhow::Result<()> {
    crate::platform::windows::host_service::resident::run(parent)
}
pub fn component_error_code(error: &anyhow::Error) -> Option<i32> {
    if error.is::<crate::platform::windows::host_service::install::ActiveSession>() {
        Some(170)
    } else if error.is::<crate::platform::windows::display::install::DriverInUse>() {
        Some(2404)
    } else if error.is::<crate::platform::windows::components::application::ImageInUse>() {
        // ERROR_SHARING_VIOLATION
        Some(32)
    } else if error.is::<crate::platform::windows::virtual_audio::install::TestSigningRequired>() {
        // ERROR_INVALID_IMAGE_HASH
        Some(577)
    } else {
        None
    }
}
pub fn input_agent(pipe: &str, parent: u32) -> anyhow::Result<()> {
    crate::features::host::input::broker::agent(pipe, parent)
}
pub fn capture_agent(pipe: &str, parent: u32) -> anyhow::Result<()> {
    crate::platform::windows::capture_service::agent(pipe, parent)
}
pub use crate::platform::windows::components::{
    Kind as ComponentKind, Operation as ComponentOperation, RemovalOptions,
};
pub fn component_operation(
    kind: ComponentKind,
    operation: ComponentOperation,
    allow_sas: bool,
    owner: Option<&str>,
    removal: RemovalOptions,
) -> anyhow::Result<bool> {
    crate::platform::windows::components::execute(kind, operation, allow_sas, owner, removal)
}
pub fn purge_machine_data() -> anyhow::Result<()> {
    crate::platform::windows::components::application::purge_machine_data()
}

/// Internal crash-recovery role of the same executable.
/// The encoder probe of a host, in a process of its own.
pub fn encoder_probe() -> anyhow::Result<()> {
    crate::features::host::desktop::probe_process()
}
pub fn display_recovery(token: &str) -> anyhow::Result<()> {
    crate::features::host::displays::recovery::watch(token)
}
