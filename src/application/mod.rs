//! Application ownership and module boundaries.

pub mod app;
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
#[cfg(windows)]
pub fn launch_installed(
    arguments: impl IntoIterator<Item = std::ffi::OsString>,
) -> anyhow::Result<()> {
    crate::platform::windows::components::application::start_installed(arguments)
}
/// 列出或接上本机正在运行的被控终端会话（命令行入口）。
///
/// 会话跑在 GUI 进程里，这里只是经 Unix socket 连过去；关闭这个终端不会
/// 结束会话。
pub fn terminal_attach(session: Option<&str>, list: bool, new: bool) -> anyhow::Result<()> {
    crate::features::host::terminal::local::run_client(session, list, new)
}

/// Hand the GUI to the installed copy when one exists. Linux has no
/// installed copy: the built binary is the application.
pub fn route_installed_gui() -> anyhow::Result<bool> {
    #[cfg(windows)]
    {
        app::maintenance::route_gui().inspect_err(app::maintenance::report_error)
    }
    #[cfg(not(windows))]
    {
        Ok(false)
    }
}
#[cfg(windows)]
pub fn uninstall_application(parent: Option<u32>) -> anyhow::Result<()> {
    app::maintenance::uninstall(parent).inspect_err(app::maintenance::report_error)
}
#[cfg(windows)]
pub fn update_application(silent: bool, no_elevate: bool) -> anyhow::Result<bool> {
    app::maintenance::update(silent, no_elevate)
}
#[cfg(windows)]
pub fn update_error_code(error: &anyhow::Error) -> i32 {
    app::maintenance::update_error_code(error)
}

// The Windows service, its resident owner and its session agents.
#[cfg(windows)]
pub fn host_service() -> anyhow::Result<()> {
    crate::platform::windows::host_service::service::run()
}
#[cfg(windows)]
pub fn configure_wol(interface: String, mac: String) -> anyhow::Result<()> {
    crate::platform::windows::wol::setup::configure_elevated(interface, mac)
}
#[cfg(windows)]
pub fn host_resident(parent: u32) -> anyhow::Result<()> {
    crate::platform::windows::host_service::resident::run(parent)
}
#[cfg(windows)]
pub fn component_error_code(error: &anyhow::Error) -> Option<i32> {
    if error.is::<crate::platform::windows::host_service::install::ActiveSession>() {
        Some(170)
    } else if error.is::<crate::platform::windows::display::install::DriverInUse>() {
        Some(2404)
    } else {
        None
    }
}
#[cfg(windows)]
pub fn input_agent(pipe: &str, parent: u32) -> anyhow::Result<()> {
    crate::features::host::input::broker::agent(pipe, parent)
}
#[cfg(windows)]
pub fn capture_agent(pipe: &str, parent: u32) -> anyhow::Result<()> {
    crate::platform::windows::capture_service::agent(pipe, parent)
}
#[cfg(windows)]
pub use crate::platform::windows::components::{
    Kind as ComponentKind, Operation as ComponentOperation, RemovalOptions,
};
#[cfg(windows)]
pub fn component_operation(
    kind: ComponentKind,
    operation: ComponentOperation,
    allow_sas: bool,
    owner: Option<&str>,
    removal: RemovalOptions,
) -> anyhow::Result<bool> {
    crate::platform::windows::components::execute(kind, operation, allow_sas, owner, removal)
}
#[cfg(windows)]
pub fn purge_machine_data() -> anyhow::Result<()> {
    crate::platform::windows::components::application::purge_machine_data()
}

/// Internal crash-recovery role of the same executable.
pub fn display_recovery(token: &str) -> anyhow::Result<()> {
    crate::features::host::displays::recovery::watch(token)
}
