//! Process-bound recovery guard primitives. No display policy lives here.
//!
//! A process is named by its PID and its start time from `/proc/<pid>/stat`,
//! so a recycled PID never passes for the owner that wrote a journal.
use anyhow::{Context, Result, bail, ensure};
use std::os::fd::AsRawFd;
use std::path::PathBuf;
use std::time::{Duration, Instant};

/// A process the guard waits on.
pub(crate) struct Handle {
    pid: u32,
    birth: u64,
}

/// Serializes display mutations between processes of the same user.
pub(crate) struct Serial(std::fs::File);
impl Serial {
    pub fn acquire() -> Result<Self> {
        let path = runtime_dir()?.join("display.lock");
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&path)
            .with_context(|| format!("无法打开显示操作锁 {}", path.display()))?;
        loop {
            // SAFETY: flock on a descriptor this function owns.
            if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } == 0 {
                return Ok(Self(file));
            }
            let error = std::io::Error::last_os_error();
            ensure!(
                error.kind() == std::io::ErrorKind::Interrupted,
                "无法取得显示操作锁：{error}"
            );
        }
    }
}
impl Drop for Serial {
    fn drop(&mut self) {
        unsafe { libc::flock(self.0.as_raw_fd(), libc::LOCK_UN) };
    }
}

/// Where the lock and readiness markers live: per user, cleared at logout.
fn runtime_dir() -> Result<PathBuf> {
    let base = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .map_or_else(crate::platform::paths::require_local_app_data, Ok)?;
    let directory = base.join("openuuyc");
    std::fs::create_dir_all(&directory)?;
    Ok(directory)
}

/// Start time in clock ticks since boot, or `None` once the process is gone.
fn birth(pid: u32) -> Result<Option<u64>> {
    let stat = match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        Ok(stat) => stat,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    // The command name is parenthesised and may itself contain spaces or
    // parentheses; fields resume after the last ')'. starttime is field 22,
    // the 20th after the name.
    let rest = stat.rsplit_once(')').context("无法解析进程状态")?.1;
    let mut fields = rest.split_ascii_whitespace();
    let state = fields.next().context("无法解析进程状态")?;
    if state == "Z" || state == "X" {
        return Ok(None);
    }
    let start = fields.nth(18).context("无法解析进程启动时间")?;
    Ok(Some(start.parse()?))
}

pub(crate) fn current_birth() -> Result<u64> {
    birth(std::process::id())?.context("无法读取本进程启动时间")
}
pub(crate) fn owner_alive(pid: u32, created: u64) -> Result<bool> {
    Ok(birth(pid)? == Some(created))
}
pub(crate) fn parent(pid: u32, created: u64) -> Result<Handle> {
    ensure!(owner_alive(pid, created)?, "显示拥有者进程已更换");
    Ok(Handle {
        pid,
        birth: created,
    })
}
/// Wait up to `timeout` ms for the process to end.
pub(crate) fn exited(handle: &Handle, timeout: u32) -> Result<bool> {
    let deadline = Instant::now() + Duration::from_millis(u64::from(timeout));
    loop {
        if !owner_alive(handle.pid, handle.birth)? {
            return Ok(true);
        }
        let now = Instant::now();
        if now >= deadline {
            return Ok(false);
        }
        std::thread::sleep((deadline - now).min(Duration::from_millis(50)));
    }
}

fn ready_marker(token: &str) -> Result<PathBuf> {
    Ok(runtime_dir()?.join(format!("display-recovery.{token}.ready")))
}
pub(crate) fn start_guard(token: &str) -> Result<()> {
    use std::os::unix::process::CommandExt;
    let marker = ready_marker(token)?;
    let _ = std::fs::remove_file(&marker);
    let mut child = std::process::Command::new(std::env::current_exe()?)
        .args(["display-recovery", token])
        // Its own process group, so a Ctrl+C aimed at the owner in a
        // terminal does not also take down the guard meant to outlive it.
        .process_group(0)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()?;
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if marker.exists() {
            let _ = std::fs::remove_file(&marker);
            // The guard outlives this call; reap it from a thread so it does
            // not linger as a zombie once it finishes.
            std::thread::spawn(move || child.wait());
            return Ok(());
        }
        if let Some(status) = child.try_wait()? {
            bail!("显示恢复守护未就绪：{status}");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let _ = child.kill();
    let _ = child.wait();
    bail!("显示恢复守护未就绪：超时")
}
pub(crate) fn ready(token: &str) -> Result<()> {
    std::fs::write(ready_marker(token)?, [])?;
    Ok(())
}
