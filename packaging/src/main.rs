#![windows_subsystem = "windows"]
use anyhow::{Context, Result, ensure};
use farwing_bootstrap as boot;
use std::{
    ffi::OsString,
    os::windows::{ffi::OsStrExt, io::AsRawHandle, process::CommandExt},
};
use windows::{
    Win32::{
        Foundation::*,
        System::{Console::*, Threading::*},
        UI::{Shell::FOLDERID_ProgramFiles, WindowsAndMessaging::*},
    },
    core::{PCWSTR, w},
};

const MANIFEST: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/manifest.json"));
const PAYLOAD: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/payload.zst"));
struct Event(HANDLE);
impl Drop for Event {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}
fn graphical(args: &[OsString]) -> bool {
    let mut i = 0;
    while i < args.len() {
        // Global options taking a value, as declared by the application's CLI.
        if args[i] == "--log-level" || args[i] == "--log-file" || args[i] == "--profile" {
            i += 2;
            continue;
        }
        if args[i].to_string_lossy().starts_with("--log-level=")
            || args[i].to_string_lossy().starts_with("--log-file=")
            || args[i].to_string_lossy().starts_with("--profile=")
        {
            i += 1;
            continue;
        }
        return args[i] == "gui" && !args[i + 1..].iter().any(|a| a == "--help" || a == "-h");
    }
    true
}
fn installed(manifest: &boot::Manifest) -> Option<boot::Image> {
    let dir = boot::known_folder(&FOLDERID_ProgramFiles)
        .ok()?
        .join(boot::PRODUCT);
    // Mirrors `components::application::RECEIPT` of the application crate.
    if std::fs::read(dir.join("owner.txt")).ok()?.as_slice() != b"Farwing deployment v1\n" {
        return None;
    }
    boot::verify(&dir.join(boot::IMAGE), manifest).ok()
}
fn execute(args: &[OsString], gui: bool) -> Result<i32> {
    let manifest: boot::Manifest = serde_json::from_slice(MANIFEST)?;
    manifest.validate()?;
    let source = std::env::current_exe()?;
    let installed_image = installed(&manifest);
    let reused_installation = installed_image.is_some();
    let image = if let Some(image) = installed_image {
        image
    } else {
        boot::extract(&manifest, PAYLOAD)?
    };
    let mut command = std::process::Command::new(&image.path);
    command
        .args(args)
        .env_remove(boot::READY_ENV)
        .env_remove(boot::ORIGIN_ENV);
    // Only an ordinary GUI uses distribution adjacency to find portable plugins.
    // Privileged/internal roles always derive identity from their real image.
    if gui {
        command.env(boot::ORIGIN_ENV, &source);
    }
    let ready = if gui {
        let name = format!(
            "{}{}-{}",
            boot::BOOT_EVENT_PREFIX,
            std::process::id(),
            uuid::Uuid::new_v4().simple()
        );
        let wide: Vec<_> = std::ffi::OsStr::new(&name)
            .encode_wide()
            .chain(Some(0))
            .collect();
        let event = Event(unsafe { CreateEventW(None, true, false, PCWSTR(wide.as_ptr()))? });
        ensure!(
            unsafe { GetLastError() } != ERROR_ALREADY_EXISTS,
            "启动交接标识冲突"
        );
        command.env(boot::READY_ENV, name);
        Some(event)
    } else {
        None
    };
    command.creation_flags(0x08000000);
    let mut child = command.spawn().context("启动Farwing失败")?;
    if gui {
        unsafe {
            let _ = AllowSetForegroundWindow(child.id());
        }
        let handles = [ready.as_ref().unwrap().0, HANDLE(child.as_raw_handle())];
        match unsafe { WaitForMultipleObjects(&handles, false, 30000) } {
            WAIT_OBJECT_0 => {
                if reused_installation {
                    let _ = boot::collect();
                }
                Ok(0)
            }
            value if value.0 == WAIT_OBJECT_0.0 + 1 => {
                let code = child.wait()?.code().unwrap_or(1);
                ensure!(code == 0, "Farwing启动失败（退出码 {code}），请查看日志");
                Ok(code)
            }
            WAIT_TIMEOUT => anyhow::bail!("Farwing尚未确认启动，程序可能仍在初始化；请查看日志。"),
            _ => Err(windows::core::Error::from_thread().into()),
        }
    } else {
        Ok(child.wait()?.code().unwrap_or(1))
    }
}
fn main() {
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    let gui = graphical(&args);
    if !gui {
        unsafe {
            let _ = AttachConsole(ATTACH_PARENT_PROCESS);
        }
    }
    let code = match execute(&args, gui) {
        Ok(code) => code,
        Err(error) => {
            if gui {
                let text: Vec<_> = format!("无法启动 Farwing\n\n{error:#}")
                    .encode_utf16()
                    .chain(Some(0))
                    .collect();
                unsafe {
                    MessageBoxW(
                        None,
                        PCWSTR(text.as_ptr()),
                        w!("Farwing"),
                        MB_OK | MB_ICONERROR,
                    );
                }
            } else {
                eprintln!("{error:#}");
            }
            1
        }
    };
    std::process::exit(code);
}
