//! SIGTERM/SIGINT 走和托盘「退出」同一条收尾路径。
//!
//! `systemctl --user stop openuuyc.service` 发的是 SIGTERM，直接死掉的话本机在线
//! 状态要等服务器超时才掉，正在进行的被控会话也不会走正常收尾；`Restart=on-failure`
//! 的单元更该看到一次干净的退出。winit 不装信号处理，事件循环也没有注册文件描述符
//! 的接口，所以按老办法：处理函数往自建管道里写一个字节（异步信号安全），一个后台
//! 线程读到就通过窗口管理器请求退出。
//!
//! Only the first signal asks for that exit. The handler puts both signals
//! back to their default action as it runs, so if the exit is refused (the
//! background shutdown failed) or hangs, the next SIGTERM or Ctrl+C ends the
//! process as it would without this module, and systemd's stop timeout is
//! never the only way out.
//!
//! The handler chains to whatever was installed before it, as
//! signal-hook-registry (tokio's signal support) does in turn for handlers
//! installed after it, so a hosted viewer's `tokio::signal::ctrl_c` keeps
//! seeing Ctrl+C. A signal the process inherited as ignored (a background
//! job, `nohup`) stays ignored.
use anyhow::{Context, Result};
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};

const SIGNALS: [libc::c_int; 2] = [libc::SIGTERM, libc::SIGINT];

/// 自建管道的写端。信号处理函数只读这个原子量。
static WAKE_WRITE: AtomicI32 = AtomicI32::new(-1);
static INSTALLED: AtomicBool = AtomicBool::new(false);

/// What each signal had before this module, written once before the handler
/// is installed and only read afterwards (by the handler too).
struct Previous(std::cell::UnsafeCell<[Option<libc::sigaction>; 2]>);
// SAFETY: written only by `install` before any handler can run, read-only after.
unsafe impl Sync for Previous {}
static PREVIOUS: Previous = Previous(std::cell::UnsafeCell::new([None, None]));
/// Which of [`SIGNALS`] this module handles.
static HANDLED: [AtomicBool; 2] = [AtomicBool::new(false), AtomicBool::new(false)];

extern "C" fn on_signal(
    signal: libc::c_int,
    info: *mut libc::siginfo_t,
    context: *mut libc::c_void,
) {
    // SAFETY: errno is thread-local; everything below is async-signal-safe
    // (sigaction, write) and restores errno for the interrupted code.
    let errno = unsafe { *libc::__errno_location() };
    for (index, other) in SIGNALS.into_iter().enumerate() {
        if HANDLED[index].load(Ordering::Relaxed) {
            // SAFETY: a zeroed sigaction is SIG_DFL with an empty mask.
            unsafe {
                let mut default: libc::sigaction = std::mem::zeroed();
                default.sa_sigaction = libc::SIG_DFL;
                libc::sigaction(other, &default, std::ptr::null_mut());
            }
        }
    }
    let fd = WAKE_WRITE.load(Ordering::Relaxed);
    if fd >= 0 {
        let byte = 1u8;
        // SAFETY: one byte from a valid address into a non-blocking pipe; a
        // full pipe already holds a pending request.
        unsafe {
            libc::write(fd, std::ptr::from_ref(&byte).cast(), 1);
        }
    }
    if let Some(index) = SIGNALS.iter().position(|&known| known == signal) {
        // SAFETY: PREVIOUS is no longer written once handlers run.
        if let Some(previous) = unsafe { (*PREVIOUS.0.get())[index] } {
            // SAFETY: calling the handler that was installed, with the
            // arguments the kernel gave this one, as its flags declare.
            unsafe { chain(&previous, signal, info, context) };
        }
    }
    // SAFETY: as above.
    unsafe { *libc::__errno_location() = errno };
}

/// Run the handler that was installed before ours, if it was a function.
unsafe fn chain(
    previous: &libc::sigaction,
    signal: libc::c_int,
    info: *mut libc::siginfo_t,
    context: *mut libc::c_void,
) {
    let handler = previous.sa_sigaction;
    if handler == libc::SIG_DFL || handler == libc::SIG_IGN {
        return;
    }
    if previous.sa_flags & libc::SA_SIGINFO != 0 {
        // SAFETY: SA_SIGINFO handlers take these three arguments.
        let handler: extern "C" fn(libc::c_int, *mut libc::siginfo_t, *mut libc::c_void) =
            unsafe { std::mem::transmute(handler) };
        handler(signal, info, context);
    } else {
        // SAFETY: plain handlers take the signal number.
        let handler: extern "C" fn(libc::c_int) = unsafe { std::mem::transmute(handler) };
        handler(signal);
    }
}

/// 让 SIGTERM、SIGINT 变成一次正常退出。进程里只会装一次。
pub(crate) fn install() -> Result<()> {
    install_with(|| super::window_manager::send(super::window_manager::Request::Exit).is_ok())
}

/// Install the handler; `notify` runs on a background thread for each
/// signal and returns false once nobody is left to tell.
fn install_with(mut notify: impl FnMut() -> bool + Send + 'static) -> Result<()> {
    anyhow::ensure!(
        !INSTALLED.swap(true, Ordering::AcqRel),
        "退出信号处理已经装过了"
    );
    let mut fds = [0i32; 2];
    // SAFETY: 传的是两个 i32 的有效地址，失败会返回负值。
    anyhow::ensure!(
        unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } == 0,
        "创建退出信号管道失败：{}",
        std::io::Error::last_os_error()
    );
    let [read, write] = fds;
    // The handler must never block on a full pipe.
    // SAFETY: `write` is the pipe's write end, just created.
    anyhow::ensure!(
        unsafe { libc::fcntl(write, libc::F_SETFL, libc::O_NONBLOCK) } == 0,
        "设置退出信号管道失败：{}",
        std::io::Error::last_os_error()
    );
    WAKE_WRITE.store(write, Ordering::Relaxed);
    std::thread::Builder::new()
        .name("openuuyc-signal".into())
        .spawn(move || {
            let mut byte = 0u8;
            // SAFETY: 阻塞读一个字节；管道写端一直开着。
            while unsafe { libc::read(read, std::ptr::from_mut(&mut byte).cast(), 1) } == 1 {
                if !notify() {
                    break;
                }
            }
        })
        .context("启动退出信号线程失败")?;
    for (index, signal) in SIGNALS.into_iter().enumerate() {
        // SAFETY: queries the current action into a zeroed struct.
        let current = unsafe {
            let mut current: libc::sigaction = std::mem::zeroed();
            anyhow::ensure!(
                libc::sigaction(signal, std::ptr::null(), &mut current) == 0,
                "读取信号处理失败：{}",
                std::io::Error::last_os_error()
            );
            current
        };
        if current.sa_sigaction == libc::SIG_IGN {
            tracing::info!(signal, "信号被启动方设为忽略，保持忽略");
            continue;
        }
        // SAFETY: no handler of ours runs before HANDLED is set below.
        unsafe { (*PREVIOUS.0.get())[index] = Some(current) };
        HANDLED[index].store(true, Ordering::Relaxed);
        // SAFETY: the handler only does async-signal-safe work; both signals
        // are blocked while it runs so one cannot interrupt the other.
        let installed = unsafe {
            let mut action: libc::sigaction = std::mem::zeroed();
            action.sa_sigaction = on_signal as *const () as libc::sighandler_t;
            action.sa_flags = libc::SA_SIGINFO | libc::SA_RESTART;
            libc::sigemptyset(&mut action.sa_mask);
            for blocked in SIGNALS {
                libc::sigaddset(&mut action.sa_mask, blocked);
            }
            libc::sigaction(signal, &action, std::ptr::null_mut())
        };
        anyhow::ensure!(
            installed == 0,
            "安装信号处理失败：{}",
            std::io::Error::last_os_error()
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    //! Signal dispositions are per process, so each scenario runs in a child
    //! process: this test binary again, filtered to this test, with the
    //! scenario in an environment variable.
    use super::*;
    use std::os::unix::process::ExitStatusExt;
    use std::sync::mpsc;
    use std::time::Duration;

    const SCENARIO: &str = "OPENUUYC_SIGNAL_SCENARIO";

    fn disposition(signal: libc::c_int) -> libc::sighandler_t {
        // SAFETY: a query into a zeroed struct.
        unsafe {
            let mut current: libc::sigaction = std::mem::zeroed();
            libc::sigaction(signal, std::ptr::null(), &mut current);
            current.sa_sigaction
        }
    }

    fn raise(signal: libc::c_int) {
        // SAFETY: raising a signal the scenario handles or expects to die of.
        unsafe { libc::raise(signal) };
    }

    /// Runs inside the child.
    fn scenario(name: &str) {
        let (sender, notified) = mpsc::channel();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let wait = |stream: &mut tokio::signal::unix::Signal| {
            runtime
                .block_on(async {
                    tokio::time::timeout(Duration::from_secs(2), stream.recv()).await
                })
                .is_ok()
        };
        match name {
            // A hosted viewer's tokio listener registered after us.
            "tokio-after" => {
                install_with(move || sender.send(()).is_ok()).unwrap();
                let mut tokio_int = runtime
                    .block_on(async {
                        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())
                    })
                    .unwrap();
                raise(libc::SIGINT);
                notified.recv_timeout(Duration::from_secs(2)).unwrap();
                assert!(wait(&mut tokio_int), "tokio missed SIGINT");
                assert_eq!(disposition(libc::SIGINT), libc::SIG_DFL);
                assert_eq!(disposition(libc::SIGTERM), libc::SIG_DFL);
                println!("first-signal-handled");
                raise(libc::SIGINT);
            }
            // A tokio listener registered before us.
            "tokio-before" => {
                let mut tokio_term = runtime
                    .block_on(async {
                        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                    })
                    .unwrap();
                install_with(move || sender.send(()).is_ok()).unwrap();
                raise(libc::SIGTERM);
                notified.recv_timeout(Duration::from_secs(2)).unwrap();
                assert!(wait(&mut tokio_term), "tokio missed SIGTERM");
                println!("first-signal-handled");
                raise(libc::SIGTERM);
            }
            // Inherited as ignored: left ignored, nothing asked.
            "ignored" => {
                // SAFETY: setting SIGINT to ignored in this child.
                unsafe { libc::signal(libc::SIGINT, libc::SIG_IGN) };
                install_with(move || sender.send(()).is_ok()).unwrap();
                raise(libc::SIGINT);
                assert!(notified.recv_timeout(Duration::from_millis(300)).is_err());
                assert_eq!(disposition(libc::SIGINT), libc::SIG_IGN);
                println!("first-signal-handled");
                return;
            }
            other => panic!("unknown scenario {other}"),
        }
        println!("survived-second-signal");
    }

    fn run_child(name: &str) -> (std::process::ExitStatus, String) {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "ui::shutdown::tests::the_first_signal_exits_cleanly_and_the_second_kills",
                "--nocapture",
                "--test-threads=1",
            ])
            .env(SCENARIO, name)
            .output()
            .unwrap();
        let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
        text.push_str(&String::from_utf8_lossy(&output.stderr));
        (output.status, text)
    }

    #[test]
    fn the_first_signal_exits_cleanly_and_the_second_kills() {
        if let Ok(name) = std::env::var(SCENARIO) {
            scenario(&name);
            return;
        }
        for (name, signal) in [
            ("tokio-after", libc::SIGINT),
            ("tokio-before", libc::SIGTERM),
        ] {
            let (status, stdout) = run_child(name);
            assert!(stdout.contains("first-signal-handled"), "{name}: {stdout}");
            assert!(
                !stdout.contains("survived-second-signal"),
                "{name}: {stdout}"
            );
            assert_eq!(status.signal(), Some(signal), "{name}: {status:?}");
        }
        let (status, stdout) = run_child("ignored");
        assert!(stdout.contains("first-signal-handled"), "ignored: {stdout}");
        assert!(status.success(), "ignored: {status:?}");
    }
}
