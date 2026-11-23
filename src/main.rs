#![cfg_attr(windows, windows_subsystem = "windows")]

#[cfg(windows)]
#[path = "main_windows.rs"]
mod windows_entry;

fn main() -> anyhow::Result<()> {
    #[cfg(windows)]
    return windows_entry::main();
    #[cfg(target_os = "linux")]
    return farwing::application::run();
}
