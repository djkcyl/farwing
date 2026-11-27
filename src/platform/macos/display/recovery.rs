//! Process-bound recovery guard primitives, shared POSIX shape: a process is
//! named by its PID and its start time, so a recycled PID never passes for
//! the owner that wrote a journal. macOS reads the start time from sysctl
//! instead of `/proc`.
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

/// Where the lock and readiness markers live: per user.
fn runtime_dir() -> Result<PathBuf> {
    let base = crate::platform::paths::require_local_app_data()?;
    let directory = base.join("openuuyc");
    std::fs::create_dir_all(&directory)?;
    Ok(directory)
}

/// Start time in nanoseconds since boot, or `None` once the process is gone.
fn birth(pid: u32) -> Result<Option<u64>> {
    // kinfo_proc supplies both the start time and the exit status in one
    // call, like reading `/proc/<pid>/stat` on Linux.
    #[repr(C)]
    struct KInfoProc {
        _layout: [u8; 640],
    }
    let mut mib = [libc::CTL_KERN, libc::KERN_PROC, libc::KERN_PROC_PID, pid as i32];
    let mut info = unsafe { std::mem::zeroed::<KInfoProc>() };
    let mut size = std::mem::size_of::<KInfoProc>();
    let result = unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            4,
            (&mut info as *mut KInfoProc).cast(),
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    };
    if result != 0 {
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() == Some(libc::ESRCH) {
            return Ok(None);
        }
        return Err(anyhow::anyhow!("读取进程 {pid} 信息失败：{error}"));
    }
    if size == 0 {
        return Ok(None);
    }
    // struct kinfo_proc (sys/proc_info.h) starts with struct extern_proc,
    // whose p_starttime is a timeval at this offset.
    const P_STARTTIME_OFFSET: usize = 288;
    let bytes = info._layout;
    let seconds = i64::from_ne_bytes(bytes[P_STARTTIME_OFFSET..P_STARTTIME_OFFSET + 8].try_into()?);
    let microseconds =
        i64::from_ne_bytes(bytes[P_STARTTIME_OFFSET + 8..P_STARTTIME_OFFSET + 16].try_into()?);
    Ok(Some((seconds * 1_000_000_000 + microseconds * 1_000) as u64))
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
        .process_group(0)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()?;
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if marker.exists() {
            let _ = std::fs::remove_file(&marker);
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
