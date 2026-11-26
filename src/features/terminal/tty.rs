//! 跑本地终端窗口时用的原始终端模式与尺寸。
//!
//! 远程终端和本机被控终端都要在本地终端里收发原始字节；Unix 与 Windows
//! 的控制接口完全不同，调用方只关心“进入原始模式、恢复、拿尺寸”。

#[cfg(unix)]
mod imp {
    use anyhow::{Result, bail};
    use std::io::Write;

    pub(crate) fn is_terminal() -> bool {
        // SAFETY: `isatty` 只读一个 fd。
        unsafe { libc::isatty(0) == 1 }
    }

    pub(crate) fn size() -> (u16, u16) {
        let mut size: libc::winsize = unsafe { std::mem::zeroed() };
        // SAFETY: stdout 是本进程的终端；`size` 是有效的 winsize。
        let filled = unsafe { libc::ioctl(1, libc::TIOCGWINSZ as libc::c_ulong, &mut size) };
        if filled != 0 || size.ws_col == 0 || size.ws_row == 0 {
            return (80, 24);
        }
        (size.ws_col, size.ws_row)
    }

    pub(crate) struct RawMode {
        original: libc::termios,
    }

    impl RawMode {
        pub(crate) fn enter() -> Result<Self> {
            // SAFETY: `tcgetattr` 只写我们提供的 termios。
            let mut original: libc::termios = unsafe { std::mem::zeroed() };
            if unsafe { libc::tcgetattr(0, &mut original) } != 0 {
                bail!("无法读取终端设置");
            }
            let mut raw = original;
            // SAFETY: `cfmakeraw` 只改结构体字段。
            unsafe { libc::cfmakeraw(&mut raw) };
            if unsafe { libc::tcsetattr(0, libc::TCSANOW, &raw) } != 0 {
                bail!("无法切换终端到原始模式");
            }
            Ok(Self { original })
        }
    }

    impl Drop for RawMode {
        fn drop(&mut self) {
            terminal_control(b"\x1b[0m\x1b[?25h");
            // SAFETY: original 来自 tcgetattr，fd 在作用域内有效。
            unsafe {
                libc::tcsetattr(0, libc::TCSANOW, &self.original);
            }
        }
    }

    pub(crate) fn terminal_control(sequence: &[u8]) {
        let mut stdout = std::io::stdout();
        let _ = stdout.write_all(sequence);
        let _ = stdout.flush();
    }
}

#[cfg(windows)]
mod imp {
    use anyhow::{Context, Result};
    use std::io::Write;
    use windows::Win32::System::Console::{
        CONSOLE_MODE, CONSOLE_SCREEN_BUFFER_INFO, ENABLE_ECHO_INPUT, ENABLE_LINE_INPUT,
        ENABLE_PROCESSED_INPUT, ENABLE_VIRTUAL_TERMINAL_INPUT, ENABLE_VIRTUAL_TERMINAL_PROCESSING,
        GetConsoleMode, GetConsoleScreenBufferInfo, GetStdHandle, STD_INPUT_HANDLE,
        STD_OUTPUT_HANDLE, SetConsoleMode,
    };

    pub(crate) fn is_terminal() -> bool {
        let Ok(handle) = (unsafe { GetStdHandle(STD_INPUT_HANDLE) }) else {
            return false;
        };
        let mut mode = CONSOLE_MODE::default();
        unsafe { GetConsoleMode(handle, &mut mode) }.is_ok()
    }

    pub(crate) fn size() -> (u16, u16) {
        let Ok(handle) = (unsafe { GetStdHandle(STD_OUTPUT_HANDLE) }) else {
            return (80, 24);
        };
        let mut info = CONSOLE_SCREEN_BUFFER_INFO::default();
        if unsafe { GetConsoleScreenBufferInfo(handle, &mut info) }.is_err() {
            return (80, 24);
        }
        let cols = (info.srWindow.Right - info.srWindow.Left + 1).max(1) as u16;
        let rows = (info.srWindow.Bottom - info.srWindow.Top + 1).max(1) as u16;
        (cols, rows)
    }

    pub(crate) struct RawMode {
        input: windows::Win32::Foundation::HANDLE,
        original: CONSOLE_MODE,
    }

    impl RawMode {
        pub(crate) fn enter() -> Result<Self> {
            let input = unsafe { GetStdHandle(STD_INPUT_HANDLE) }.context("读取标准输入失败")?;
            let mut original = CONSOLE_MODE::default();
            unsafe { GetConsoleMode(input, &mut original) }.context("读取控制台模式失败")?;
            let raw = CONSOLE_MODE(
                (original.0
                    & !(ENABLE_LINE_INPUT.0 | ENABLE_ECHO_INPUT.0 | ENABLE_PROCESSED_INPUT.0))
                    | ENABLE_VIRTUAL_TERMINAL_INPUT.0,
            );
            unsafe { SetConsoleMode(input, raw) }.context("切换控制台到原始模式失败")?;
            if let Ok(output) = unsafe { GetStdHandle(STD_OUTPUT_HANDLE) } {
                let mut mode = CONSOLE_MODE::default();
                if unsafe { GetConsoleMode(output, &mut mode) }.is_ok() {
                    mode.0 |= ENABLE_VIRTUAL_TERMINAL_PROCESSING.0;
                    let _ = unsafe { SetConsoleMode(output, mode) };
                }
            }
            Ok(Self { input, original })
        }
    }

    impl Drop for RawMode {
        fn drop(&mut self) {
            terminal_control(b"\x1b[0m\x1b[?25h");
            let _ = unsafe { SetConsoleMode(self.input, self.original) };
        }
    }

    pub(crate) fn terminal_control(sequence: &[u8]) {
        let mut stdout = std::io::stdout();
        let _ = stdout.write_all(sequence);
        let _ = stdout.flush();
    }
}

pub(crate) use imp::{RawMode, is_terminal, size};
