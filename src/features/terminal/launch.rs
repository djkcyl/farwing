//! 在系统默认终端模拟器里启动本程序的子命令。
//!
//! 终端窗口不是 GUI 的子窗口：关掉控制中心不应该带走正在跑的 shell，所以
//! 这里始终启动一个新的模拟器进程。Linux 先看 `$TERMINAL` 再按常见列表找，
//! Windows 交给 `start`，macOS 用 Terminal.app。

use anyhow::{Context, Result};
use std::ffi::OsString;
use std::process::Command;

#[cfg(target_os = "linux")]
fn terminal_emulator() -> Option<(String, Vec<String>)> {
    if let Some(setting) = std::env::var_os("TERMINAL").and_then(|value| value.into_string().ok()) {
        let mut words = setting.split_whitespace();
        if let Some(program) = words.next()
            && in_path(program)
        {
            return Some((program.to_owned(), words.map(str::to_owned).collect()));
        }
    }
    const CANDIDATES: &[(&str, &[&str])] = &[
        ("xdg-terminal-exec", &[]),
        ("foot", &["-e"]),
        ("alacritty", &["-e"]),
        ("kitty", &["-e"]),
        ("konsole", &["-e"]),
        ("ghostty", &["-e"]),
        ("wezterm", &["start", "--"]),
        ("gnome-terminal", &["--"]),
        ("xterm", &["-e"]),
    ];
    CANDIDATES
        .iter()
        .find(|(program, _)| in_path(program))
        .map(|(program, args)| {
            (
                (*program).to_owned(),
                args.iter().map(|arg| (*arg).to_owned()).collect(),
            )
        })
}

#[cfg(target_os = "linux")]
fn in_path(program: &str) -> bool {
    let path = std::path::Path::new(program);
    if path.components().count() > 1 {
        return path.is_file();
    }
    std::env::var_os("PATH").is_some_and(|paths| {
        std::env::split_paths(&paths).any(|directory| {
            let candidate = directory.join(program);
            candidate.is_file() && is_executable(&candidate)
        })
    })
}

#[cfg(target_os = "linux")]
fn is_executable(path: &std::path::Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .map(|metadata| metadata.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

/// 打开一个终端窗口运行 `args`。返回的进程可能一启动就退出（把窗口交给
/// 已有实例的模拟器），所以调用方不要等待终端窗口本身。
pub(crate) fn open(args: Vec<OsString>) -> Result<()> {
    #[cfg(target_os = "linux")]
    {
        let (program, prefix) = terminal_emulator().context("找不到可用的终端模拟器")?;
        let mut command = Command::new(&program);
        command.args(prefix);
        command.args(args);
        let mut child = command
            .spawn()
            .with_context(|| format!("启动 {program} 失败"))?;
        std::thread::Builder::new()
            .name("terminal emulator".into())
            .spawn(move || {
                let _ = child.wait();
            })
            .context("无法等待终端模拟器退出")?;
        Ok(())
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        let mut line = String::new();
        for arg in args {
            line.push('"');
            line.push_str(&arg.to_string_lossy().replace('"', "\\\""));
            line.push_str("\" ");
        }
        Command::new("cmd.exe")
            .raw_arg(format!("/C start \"\" cmd.exe /C \"{line}\""))
            .spawn()
            .context("启动默认终端失败")?;
        Ok(())
    }
    #[cfg(target_os = "macos")]
    {
        let mut command = String::new();
        for arg in args {
            command.push_str(&shell_quote(&arg.to_string_lossy()));
            command.push(' ');
        }
        let script = format!(
            "tell application \"Terminal\" to do script \"{}\"",
            command.replace('\\', "\\\\").replace('"', "\\\"")
        );
        Command::new("osascript")
            .args(["-e", &script])
            .spawn()
            .context("启动 Terminal.app 失败")?;
        Ok(())
    }
}

#[cfg(target_os = "macos")]
fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}
