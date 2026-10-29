//! Ordinary-user execution endpoint. Connections own tasks, never this process.
use super::{
    pipe::{Handle, Pipe},
    process, vault,
};
use crate::i18n::t;
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};
static JOBS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
struct Seen {
    pid: u32,
    process: Handle,
}
// Kernel process handles may be waited on from any thread; the mutex owns close.
unsafe impl Send for Seen {}
static LAST_PROCESS: std::sync::Mutex<Option<Seen>> = std::sync::Mutex::new(None);
fn remember(pid: u32, process: Handle) {
    *LAST_PROCESS.lock().unwrap_or_else(|e| e.into_inner()) = Some(Seen { pid, process });
}
/// `pid` is the signed-in user's backend in `session`: this image, that
/// session, the user's own logon (needs the service's privileges).
pub(super) fn verify(pid: u32, session: u32) -> Result<()> {
    ensure!(
        process::session(pid)? == session,
        "{}",
        t().system.service_user_backend_session_mismatch
    );
    process::verify_image(pid)?;
    ensure!(
        process_identity(pid)? == user_identity(session)?,
        "{}",
        t().system.service_user_backend_identity_mismatch
    );
    Ok(())
}
pub(crate) struct Observer(Handle);
unsafe impl Send for Observer {}
impl Observer {
    pub fn alive(&self) -> bool {
        (unsafe { windows::Win32::System::Threading::WaitForSingleObject(self.0.0, 0) })
            == windows::Win32::Foundation::WAIT_TIMEOUT
    }
}
pub(crate) fn observe() -> Result<Option<Observer>> {
    use windows::Win32::{Foundation::*, System::Threading::*};
    let current = LAST_PROCESS.lock().unwrap_or_else(|e| e.into_inner());
    let Some(current) = current.as_ref() else {
        return Ok(None);
    };
    let mut handle = HANDLE::default();
    unsafe {
        DuplicateHandle(
            GetCurrentProcess(),
            current.process.0,
            GetCurrentProcess(),
            &mut handle,
            0,
            false,
            DUPLICATE_SAME_ACCESS,
        )?;
    }
    Ok(Some(Observer(Handle(handle))))
}
struct Job;
impl Job {
    fn new() -> Self {
        JOBS.fetch_add(1, Ordering::AcqRel);
        Self
    }
}
impl Drop for Job {
    fn drop(&mut self) {
        JOBS.fetch_sub(1, Ordering::AcqRel);
    }
}
static STARTED: AtomicBool = AtomicBool::new(false);
static STOPPING: AtomicBool = AtomicBool::new(false);
pub(crate) fn active() -> bool {
    STARTED.load(Ordering::Acquire)
}
pub(crate) fn permitted() -> bool {
    !STOPPING.load(Ordering::Acquire)
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub(crate) enum Role {
    Clipboard,
    Files,
}
impl Role {
    fn prefix(self) -> String {
        match self {
            Self::Clipboard => crate::product::pipe_name("Clipboard."),
            Self::Files => crate::product::pipe_name("Files."),
        }
    }
    fn run(self, pipe: &str, parent: u32) -> Result<()> {
        match self {
            Self::Clipboard => crate::features::host::clipboard::agent::run(pipe, parent),
            Self::Files => crate::features::file_transfer::host::agent::run(pipe, parent),
        }
    }
}
#[derive(Serialize, Deserialize)]
enum Request {
    Ping,
    Open { role: Role, pipe: String },
}
#[derive(Serialize, Deserialize)]
enum Reply {
    Ready { active: usize },
    Finished(Option<String>),
}
fn name(session: u32) -> String {
    crate::product::pipe_name(&format!("UserBackend.{session}"))
}

fn authorize(pipe: &Pipe, _session: u32) -> Result<u32> {
    use windows::Win32::{
        Security::*,
        System::{Pipes::ImpersonateNamedPipeClient, Threading::*},
    };
    struct Revert;
    impl Drop for Revert {
        fn drop(&mut self) {
            // A user worker must never continue with the caller's SYSTEM token.
            if unsafe { RevertToSelf() }.is_err() {
                std::process::abort();
            }
        }
    }
    let pid = pipe.peer_pid(true)?;
    unsafe {
        ImpersonateNamedPipeClient(pipe.handle.0)?;
    }
    let _revert = Revert;
    let mut token = windows::Win32::Foundation::HANDLE::default();
    unsafe {
        OpenThreadToken(GetCurrentThread(), TOKEN_QUERY, true, &mut token)?;
    }
    let token = Handle(token);
    ensure!(
        vault::token_sid(token.0)? == "S-1-5-18",
        "用户后台仅接受系统执行进程"
    );
    Ok(pid)
}

pub(crate) struct Server {
    stop: Arc<AtomicBool>,
    worker: Option<std::thread::JoinHandle<()>>,
}
impl Server {
    pub fn start() -> Result<Self> {
        ensure!(
            vault::sid(std::process::id())? != "S-1-5-18",
            "{}",
            t().system.service_user_backend_system_identity
        );
        let session = process::session(std::process::id())?;
        let listener = Pipe::listener(&name(session), true, 16, true)?;
        STARTED.store(true, Ordering::Release);
        STOPPING.store(false, Ordering::Release);
        let stop = Arc::new(AtomicBool::new(false));
        let stopped = stop.clone();
        let worker = std::thread::Builder::new()
            .name("user-backend".into())
            .spawn(move || {
                let runtime = tokio::runtime::Builder::new_multi_thread()
                    .worker_threads(2)
                    .enable_all()
                    .build();
                let Ok(runtime) = runtime else {
                    tracing::error!("user backend runtime unavailable");
                    return;
                };
                let result = std::thread::scope(|scope| -> Result<()> {
                    let mut listener = listener;
                    let mut tasks = Vec::new();
                    while !stopped.load(Ordering::Acquire) {
                        tasks.retain(|t: &std::thread::ScopedJoinHandle<'_, ()>| !t.is_finished());
                        if tasks.len() >= 15 {
                            std::thread::sleep(Duration::from_millis(20));
                            continue;
                        }
                        if listener
                            .accept(|| !stopped.load(Ordering::Acquire))
                            .is_err()
                        {
                            continue;
                        }
                        let next = Pipe::listener(&name(session), true, 16, false)?;
                        let pipe = std::mem::replace(&mut listener, next);
                        let runtime = runtime.handle();
                        tasks.push(scope.spawn(move || {
                            let _runtime = runtime.enter();
                            let result = (|| -> Result<()> {
                                loop {
                                    if !permitted() {
                                        return Ok(());
                                    }
                                    if !pipe.available_within(super::resident::IDLE_WAIT)? {
                                        continue;
                                    }
                                    let request: Request = pipe.receive(permitted)?;
                                    // Named-pipe impersonation uses the security
                                    // context of the last message read.
                                    let parent = authorize(&pipe, session)?;
                                    match request {
                                        Request::Ping => {
                                            pipe.send(
                                                &Reply::Ready {
                                                    active: JOBS.load(Ordering::Acquire),
                                                },
                                                || true,
                                            )?;
                                        }
                                        Request::Open {
                                            role,
                                            pipe: channel,
                                        } => {
                                            ensure!(
                                                pipe.peer_session(true)? == session,
                                                "用户后台请求跨Windows会话"
                                            );
                                            let resident = std::fs::read_to_string(
                                                super::install::directory()?.join("resident.pid"),
                                            )?
                                            .trim()
                                            .parse::<u32>()?;
                                            ensure!(
                                                parent == resident,
                                                "用户工作不属于当前系统执行进程"
                                            );
                                            let suffix = channel
                                                .strip_prefix(role.prefix().as_str())
                                                .context("用户工作通道类型不符")?;
                                            ensure!(
                                                uuid::Uuid::parse_str(suffix).is_ok(),
                                                "用户工作通道标识无效"
                                            );
                                            ensure!(
                                                session == process::active_session(),
                                                "用户会话已切换"
                                            );
                                            ensure!(permitted(), "用户后台正在退出");
                                            let _job = Job::new();
                                            pipe.send(
                                                &Reply::Ready {
                                                    active: JOBS.load(Ordering::Acquire),
                                                },
                                                || true,
                                            )?;
                                            let result = role.run(&channel, parent);
                                            return pipe.send(
                                                &Reply::Finished(
                                                    result.err().map(|e| format!("{e:#}")),
                                                ),
                                                || true,
                                            );
                                        }
                                    }
                                }
                            })();
                            if let Err(error) = result {
                                tracing::debug!(%error,"user backend task ended");
                            }
                        }));
                    }
                    Ok(())
                });
                if let Err(error) = result {
                    tracing::error!(%error,"user backend stopped");
                }
            })?;
        Ok(Self {
            stop,
            worker: Some(worker),
        })
    }
}
impl Drop for Server {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        STOPPING.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}
impl Drop for Lease {
    fn drop(&mut self) {
        // The role's private data pipe is dropped first by its owner. Wait for
        // the handler's resource teardown, not for the persistent GUI to exit.
        let _ = self
            .control
            .receive_timeout::<Reply>(Duration::from_secs(2), || true);
    }
}

pub(crate) struct Lease {
    control: Pipe,
    pub pid: u32,
}
impl Lease {
    pub fn connect(
        role: Role,
        channel: &str,
        session: u32,
        allowed: impl Fn() -> bool,
    ) -> Result<Self> {
        let started = Instant::now();
        let control = loop {
            ensure!(
                allowed() && session == process::active_session(),
                "{}",
                t().system.service_user_task_cancelled
            );
            if let Some(pipe) = Pipe::client(&name(session))? {
                break pipe;
            }
            ensure!(
                started.elapsed() < Duration::from_secs(5),
                "{}",
                t().system.service_user_backend_not_ready
            );
            std::thread::sleep(Duration::from_millis(20));
        };
        let pid = control.peer_pid(false)?;
        ensure!(
            control.peer_session(false)? == session,
            "{}",
            t().system.service_user_backend_session_mismatch
        );
        // The service checks the user's identity and hands over a handle
        // to wait on the backend.
        let process = super::requests::user_backend(session, pid)?;
        control.send(
            &Request::Open {
                role,
                pipe: channel.into(),
            },
            &allowed,
        )?;
        ensure!(
            matches!(control.receive::<Reply>(&allowed)?, Reply::Ready { .. }),
            "{}",
            t().system.service_user_backend_refused
        );
        remember(pid, process);
        Ok(Self { control, pid })
    }
    pub fn alive(&self) -> bool {
        self.control.queued_bytes().is_ok()
    }
    pub fn finish(&self) -> Result<()> {
        match self
            .control
            .receive_timeout::<Reply>(Duration::from_secs(5), || true)?
        {
            Reply::Finished(None) => Ok(()),
            Reply::Finished(Some(error)) => anyhow::bail!(error),
            _ => anyhow::bail!("{}", t().system.service_user_task_not_cleaned),
        }
    }
}
fn user_sid(session: u32) -> Result<String> {
    Ok(user_identity(session)?.0)
}
type Identity = (String, u64, bool);
fn token_identity(token: windows::Win32::Foundation::HANDLE) -> Result<Identity> {
    use windows::Win32::Security::*;
    let mut stats = TOKEN_STATISTICS::default();
    let mut elevation = TOKEN_ELEVATION::default();
    let mut needed = 0;
    unsafe {
        GetTokenInformation(
            token,
            TokenStatistics,
            Some((&mut stats as *mut TOKEN_STATISTICS).cast()),
            std::mem::size_of_val(&stats) as u32,
            &mut needed,
        )?;
        GetTokenInformation(
            token,
            TokenElevation,
            Some((&mut elevation as *mut TOKEN_ELEVATION).cast()),
            std::mem::size_of_val(&elevation) as u32,
            &mut needed,
        )?;
    }
    Ok((
        vault::token_sid(token)?,
        (u64::from(stats.AuthenticationId.HighPart as u32) << 32)
            | u64::from(stats.AuthenticationId.LowPart),
        elevation.TokenIsElevated != 0,
    ))
}
fn process_identity(pid: u32) -> Result<Identity> {
    use windows::Win32::{Security::TOKEN_QUERY, System::Threading::*};
    let process = Handle(unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid)? });
    let mut token = windows::Win32::Foundation::HANDLE::default();
    unsafe {
        OpenProcessToken(process.0, TOKEN_QUERY, &mut token)?;
    }
    let token = Handle(token);
    token_identity(token.0)
}
fn user_identity(session: u32) -> Result<Identity> {
    let mut token = windows::Win32::Foundation::HANDLE::default();
    unsafe {
        windows::Win32::System::RemoteDesktop::WTSQueryUserToken(session, &mut token)?;
    }
    let token = Handle(token);
    token_identity(token.0)
}

pub(crate) fn idle() -> Result<bool> {
    use windows::Win32::{Foundation::WAIT_TIMEOUT, System::Threading::WaitForSingleObject};
    let session = process::session(std::process::id())?;
    let mut seen = LAST_PROCESS.lock().unwrap_or_else(|e| e.into_inner());
    let alive = seen
        .as_ref()
        .is_some_and(|s| unsafe { WaitForSingleObject(s.process.0, 0) } == WAIT_TIMEOUT);
    let Some(pipe) = Pipe::client(&name(session))? else {
        return Ok(!alive);
    };
    let pid = pipe.peer_pid(false)?;
    if alive {
        ensure!(
            seen.as_ref().is_some_and(|s| s.pid == pid),
            "{}",
            t().system.service_user_backend_still_running
        );
    } else {
        ensure!(
            pipe.peer_session(false)? == session,
            "{}",
            t().system.service_user_backend_session_mismatch
        );
        drop(seen);
        let process = super::requests::user_backend(session, pid)?;
        remember(pid, process);
        seen = LAST_PROCESS.lock().unwrap_or_else(|e| e.into_inner());
    }
    pipe.send(&Request::Ping, || true)?;
    let reply = pipe.receive::<Reply>(|| true)?;
    drop(seen);
    match reply {
        Reply::Ready { active } => Ok(active == 0),
        _ => anyhow::bail!("{}", t().system.service_user_backend_not_idle),
    }
}

pub(crate) fn supervise(running: impl Fn() -> bool) -> Result<()> {
    let mut launched: Option<(u32, process::Agent)> = None;
    let mut observer: Option<(u32, Pipe)> = None;
    while running() {
        let session = process::active_session();
        if launched
            .as_ref()
            .is_some_and(|(s, a)| *s != session || !a.alive())
        {
            launched = None;
        }
        if observer.as_ref().is_some_and(|(s, _)| *s != session) {
            observer = None;
        }
        if !super::resident::paused()? && vault::applies()? && user_sid(session).is_ok() {
            if observer.is_none() {
                if let Ok(Some(pipe)) = Pipe::client(&name(session)) {
                    observer = Some((session, pipe));
                }
            }
            let available = observer.as_ref().is_some_and(|(_, pipe)| {
                pipe.send(&Request::Ping, &running)
                    .and_then(|_| pipe.receive::<Reply>(&running))
                    .is_ok()
            });
            if !available {
                observer = None;
                if launched.is_none() {
                    match process::Agent::start_user(session) {
                        Ok(agent) => launched = Some((session, agent)),
                        Err(error) => tracing::debug!(%error,"user backend startup deferred"),
                    }
                }
            }
        }
        std::thread::sleep(Duration::from_secs(1));
    }
    Ok(())
}
