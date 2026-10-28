//! Single unattended account owner, independent of the ordinary desktop UI.
use super::{pipe::Pipe, process, vault};
use crate::i18n::t;
use crate::{
    account::{
        auth::{KeyringSessionStore, NativeIdentity, SessionStore},
        client::AuthenticatedClient,
    },
    features::host,
    session::{
        device_session::DeviceRuntime,
        host_client::HostClient,
        presence::{ActivePresence, PresenceEvent, PresenceState},
    },
};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU32, Ordering},
    },
    time::Duration,
};

static CONTROL: std::sync::LazyLock<String> =
    std::sync::LazyLock::new(|| crate::product::pipe_name("Resident.Control.v1"));
static SESSION: std::sync::LazyLock<String> =
    std::sync::LazyLock::new(|| crate::product::pipe_name("Resident.Session.v1"));
static OWNER: AtomicBool = AtomicBool::new(false);
pub(crate) fn is_owner() -> bool {
    OWNER.load(Ordering::Acquire)
}
/// The background service hosts this profile. Machine-wide components
/// belong to the default profile; other profiles always run portable.
pub(crate) fn managed() -> bool {
    crate::product::is_default_profile() && !is_owner() && vault::applies().unwrap_or(false)
}

#[derive(Serialize, Deserialize)]
pub(crate) enum Request {
    Resume,
    Pause,
    Quiescent,
    PrepareUpdate,
    CancelUpdate,
    Snapshot {
        ui: bool,
        #[serde(default)]
        device_cursor: Option<crate::account::device_change::relay::Cursor>,
        /// The device centre's interface language: texts this process
        /// composes for it (status, errors) use it too.
        #[serde(default)]
        language: Option<String>,
    },
    Assist {
        account: String,
        action: host::assist::Action,
    },
    RefreshPublication,
    RefreshWol,
    WolSetup {
        account: String,
        action: host::wol::setup::Action,
    },
    Initialize {
        force: bool,
    },
    Policy(crate::account::api::DevicePolicy),
    Name {
        device: String,
        value: String,
    },
    Settings {
        account: String,
        settings: host::HostSettings,
        assistance: host::assist::Settings,
    },
    Disconnect {
        account: String,
        session: String,
    },
    BlockControl {
        account: String,
        session: String,
        blocked: bool,
    },
    /// Host-side direct connection state (`crate::transport::direct::managed`).
    DirectState,
    /// Change host-side direct state: pairing code, paired controllers,
    /// network switches. Validated here, not trusted from the client.
    Direct {
        account: String,
        action: crate::transport::direct::managed::Action,
    },
    Retry {
        account: String,
    },
    /// Try the server again now instead of waiting.
    Reconnect,
    Retire {
        account: String,
    },
}
#[derive(Serialize, Deserialize)]
pub(crate) struct Snapshot {
    #[serde(default)]
    pub device_changes: Option<crate::account::device_change::relay::Batch>,
    pub assistance: host::assist::Snapshot,
    pub publication: crate::account::reporting::Snapshot,
    pub account: String,
    pub online: PresenceState,
    pub settings: host::HostSettings,
    pub status: host::Status,
    pub capabilities: Option<host::desktop::Capabilities>,
    /// What the hosting process uses for direct connections.
    #[serde(default)]
    pub direct: crate::transport::direct::managed::HostDirect,
    /// When this process next tries the server again, if it is waiting.
    #[serde(default)]
    pub retry_in_ms: Option<u64>,
    /// Notification pushes this process received (`account::notices`).
    #[serde(default)]
    pub notices: u64,
}
#[derive(Serialize, Deserialize)]
pub(crate) enum Reply {
    Done,
    Snapshot(Box<Snapshot>),
    Identity(Box<NativeIdentity>),
    DirectState {
        account: String,
        state: Box<crate::transport::direct::managed::HostDirect>,
    },
    Error(String),
    ApiError {
        code: crate::account::api::ErrorCode,
        message: String,
        #[serde(default)]
        reason: Option<String>,
        #[serde(default)]
        details: serde_json::Value,
    },
}
impl Reply {
    fn from_error(error: anyhow::Error) -> Self {
        match error.downcast::<crate::account::api::ApiFailure>() {
            Ok(error) => Self::ApiError {
                code: error.code,
                message: error.message,
                reason: error.reason,
                details: error.details,
            },
            Err(error) => Self::Error(format!("{error:#}")),
        }
    }
    pub fn checked(self) -> Result<Self> {
        match self {
            Self::Error(e) => anyhow::bail!(e),
            Self::ApiError {
                code,
                message,
                reason,
                details,
            } => {
                let mut failure = crate::account::api::ApiFailure::new(code, message);
                failure.reason = reason;
                failure.details = details;
                Err(failure.into())
            }
            other => Ok(other),
        }
    }
}
/// How long a request-serving connection waits for its next message before
/// it looks at shutdown again: such connections stay open while idle.
pub(super) const IDLE_WAIT: u32 = 100;

pub(crate) fn call(request: Request) -> Result<Reply> {
    static CONTROL_CONNECTION: std::sync::Mutex<Option<Pipe>> = std::sync::Mutex::new(None);
    static INITIALIZER_CONNECTION: std::sync::Mutex<Option<Pipe>> = std::sync::Mutex::new(None);
    // Registration can wait on the network. It must not block permission
    // revocation, disconnect or explicit exit on the management connection.
    let slot = if matches!(
        &request,
        Request::Initialize { .. } | Request::Policy(_) | Request::Name { .. }
    ) {
        &INITIALIZER_CONNECTION
    } else {
        &CONTROL_CONNECTION
    };
    let mut connection = slot.lock().unwrap_or_else(|e| e.into_inner());
    if connection
        .as_ref()
        .is_some_and(|pipe| pipe.queued_bytes().is_err())
    {
        connection.take();
    }
    if connection.is_none() {
        let started = std::time::Instant::now();
        let pipe = loop {
            match Pipe::client(&CONTROL) {
                Ok(Some(pipe)) => break pipe,
                Ok(None) if started.elapsed() < Duration::from_secs(5) => (),
                Err(error)
                    if started.elapsed() < Duration::from_secs(5)
                        && error
                            .downcast_ref::<windows::core::Error>()
                            .is_some_and(|e| {
                                e.code() == windows::Win32::Foundation::ERROR_PIPE_BUSY.to_hresult()
                            }) =>
                {
                    ()
                }
                Ok(None) => anyhow::bail!("{}", t().system.service_resident_not_started),
                Err(error) => return Err(error),
            }
            std::thread::sleep(Duration::from_millis(50));
        };
        super::install::verify_resident(pipe.peer_pid(false)?)?;
        *connection = Some(pipe);
    }
    let pipe = connection.as_ref().unwrap();
    let result = pipe
        .send(&request, || true)
        // Includes the bounded session-start wait before a request is forwarded.
        .and_then(|_| pipe.receive_timeout::<Reply>(Duration::from_secs(45), || true));
    if result.is_err() {
        connection.take();
    }
    result?.checked()
}
pub(crate) async fn request(request: Request) -> Result<Reply> {
    tokio::task::spawn_blocking(move || call(request))
        .await
        .context(t().system.service_resident_request_interrupted)?
}

/// A missing pipe is not proof that the host has stopped. Check SCM before
/// skipping retirement, including when the service stops during the request.
pub(crate) async fn pause_for_exit() -> Result<()> {
    tokio::task::spawn_blocking(|| {
        if !super::install::running()? {
            return Ok(());
        }
        match call(Request::Pause) {
            Ok(Reply::Done) => Ok(()),
            Ok(_) => anyhow::bail!("{}", t().system.service_resident_invalid_exit_reply),
            Err(error) => match super::install::running() {
                Ok(false) => Ok(()),
                _ => Err(error),
            },
        }
    })
    .await
    .context(t().system.service_resident_exit_interrupted)?
}

fn authorize(pipe: &Pipe) -> Result<()> {
    let pid = pipe.peer_pid(true)?;
    // This management surface exposes only the installing user's own account
    // state and typed controls. It cannot open arbitrary desktops, inject input
    // or launch executables. Same-user clients may update to a newer build;
    // privileged capture/input pipes retain their strict image verification.
    let sid = vault::sid(pid)?;
    ensure!(
        sid == "S-1-5-18" || Some(sid) == vault::owner()?,
        "{}",
        crate::i18n::t().system.service_user_not_allowed
    );
    Ok(())
}
pub(crate) fn supervise(running: impl Fn() -> bool + Sync) -> Result<()> {
    let paused = AtomicBool::new(boot_pause(None)?);
    let child_pid = AtomicU32::new(0);
    let stopped = AtomicBool::new(false);
    let alive = || running() && !stopped.load(Ordering::Acquire);
    let pid_file = super::install::directory()?.join("resident.pid");
    std::thread::scope(|scope| -> Result<()> {
        let server = scope.spawn(|| {
            let result = serve_control(&alive, &paused, &child_pid);
            stopped.store(true, Ordering::Release);
            result
        });
        let mut child: Option<(u32, process::Agent)> = None;
        let mut next_start = std::time::Instant::now();
        let result = (|| -> Result<()> {
            while alive() {
                let session = process::active_session();
                let enabled = session != u32::MAX;
                if child
                    .as_ref()
                    .is_some_and(|(id, agent)| !enabled || *id != session || !agent.alive())
                {
                    if let Some((_, agent)) = child.take() {
                        agent.stop_gracefully();
                    }
                    child_pid.store(0, Ordering::Release);
                    let _ = std::fs::remove_file(&pid_file);
                    next_start = std::time::Instant::now() + Duration::from_secs(1);
                }
                if enabled && child.is_none() && std::time::Instant::now() >= next_start {
                    match process::Agent::start_resident(session) {
                        Ok(agent) => {
                            std::fs::write(&pid_file, agent.pid.to_string())?;
                            child_pid.store(agent.pid, Ordering::Release);
                            child = Some((session, agent));
                        }
                        Err(error) => {
                            tracing::warn!(%error, "unattended session could not start");
                            next_start = std::time::Instant::now() + Duration::from_secs(3);
                        }
                    }
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            Ok(())
        })();
        stopped.store(true, Ordering::Release);
        if let Some((_, agent)) = child {
            agent.stop_gracefully();
        }
        child_pid.store(0, Ordering::Release);
        let _ = std::fs::remove_file(&pid_file);
        result.and(
            server
                .join()
                .map_err(|_| anyhow::anyhow!("后台控制线程异常"))?,
        )
    })
}
fn serve_control(
    running: &(impl Fn() -> bool + Sync),
    paused: &AtomicBool,
    child: &AtomicU32,
) -> Result<()> {
    let channel_stop = tokio_util::sync::CancellationToken::new();
    let still_running = || running() && !channel_stop.is_cancelled();
    std::thread::scope(|scope| -> Result<()> {
        let _cancel = channel_stop.clone().drop_guard();
        let running = &still_running;
        let mut workers = Vec::new();
        let mut listener = Pipe::listener(&CONTROL, true, 8, true)?;
        while running() {
            workers.retain(|worker: &std::thread::ScopedJoinHandle<'_, ()>| !worker.is_finished());
            if workers.len() >= 7 {
                std::thread::sleep(Duration::from_millis(50));
                continue;
            }
            if listener.accept(running).is_err() {
                continue;
            }
            let next = Pipe::listener(&CONTROL, true, 8, false)?;
            let pipe = std::mem::replace(&mut listener, next);
            workers.push(scope.spawn(move || {
                if let Err(error) = control_connection(pipe, running, paused, child) {
                    tracing::debug!(%error, "resident control connection ended");
                }
            }));
        }
        Ok(())
    })
}
fn control_connection(
    pipe: Pipe,
    running: &impl Fn() -> bool,
    paused: &AtomicBool,
    child: &AtomicU32,
) -> Result<()> {
    authorize(&pipe)?;
    let mut forward: Option<(u32, Pipe)> = None;
    while running() {
        // An idle management connection must not retain a previous session's
        // private pipe while its replacement publishes a new listener.
        if forward
            .as_ref()
            .is_some_and(|(pid, _)| *pid != child.load(Ordering::Acquire))
        {
            forward.take();
        }
        if !pipe.available_within(IDLE_WAIT)? {
            continue;
        }
        let result = (|| -> Result<Reply> {
            let request: Request = pipe.receive_timeout(Duration::from_secs(5), running)?;
            match request {
                Request::Resume => {
                    boot_pause(Some(false))?;
                    paused.store(false, Ordering::Release);
                    Ok(Reply::Done)
                }
                Request::Pause => {
                    boot_pause(Some(true))?;
                    paused.store(true, Ordering::Release);
                    if child.load(Ordering::Acquire) == 0 {
                        Ok(Reply::Done)
                    } else {
                        let (_, target) =
                            wait_for_session(child, &AtomicBool::new(false), running)?;
                        target.send(&Request::Pause, running)?;
                        target.receive_timeout(Duration::from_secs(12), running)
                    }
                }
                Request::Quiescent => {
                    if child.load(Ordering::Acquire) == 0 {
                        Ok(Reply::Done)
                    } else {
                        let (_, target) =
                            wait_for_session(child, &AtomicBool::new(false), running)?;
                        target.send(&Request::Quiescent, running)?;
                        target.receive_timeout(Duration::from_secs(5), running)
                    }
                }
                request => {
                    ensure!(
                        !paused.load(Ordering::Acquire),
                        "{}",
                        t().system.service_resident_exited
                    );
                    if forward
                        .as_ref()
                        .is_some_and(|(_, pipe)| pipe.queued_bytes().is_err())
                    {
                        forward.take();
                    }
                    if forward.is_none() {
                        forward = Some(wait_for_session(child, paused, running)?);
                    }
                    let target = &forward.as_ref().unwrap().1;
                    let result = target
                        .send(&request, running)
                        .and_then(|_| target.receive_timeout(Duration::from_secs(30), running));
                    if result.is_err() {
                        forward.take();
                    }
                    result
                }
            }
        })();
        let reply = result.unwrap_or_else(Reply::from_error);
        pipe.send(&reply, running)?;
    }
    Ok(())
}
fn wait_for_session(
    child: &AtomicU32,
    paused: &AtomicBool,
    running: &impl Fn() -> bool,
) -> Result<(u32, Pipe)> {
    let started = std::time::Instant::now();
    let mut waited = false;
    loop {
        ensure!(
            running() && !paused.load(Ordering::Acquire),
            "{}",
            t().system.service_resident_start_cancelled
        );
        let pid = child.load(Ordering::Acquire);
        if pid != 0 {
            match Pipe::client(&SESSION) {
                Ok(Some(pipe)) => {
                    if child.load(Ordering::Acquire) == pid {
                        ensure!(
                            pipe.peer_pid(false)? == pid,
                            "{}",
                            t().system.service_resident_identity_mismatch
                        );
                        if waited {
                            tracing::info!(
                                pid,
                                waited_ms = started.elapsed().as_millis(),
                                "resident request waited for session readiness"
                            );
                        }
                        return Ok((pid, pipe));
                    }
                }
                Ok(None) => (),
                Err(error)
                    if error
                        .downcast_ref::<windows::core::Error>()
                        .is_some_and(|e| {
                            e.code() == windows::Win32::Foundation::ERROR_PIPE_BUSY.to_hresult()
                        }) =>
                {
                    ()
                }
                Err(error) => return Err(error),
            }
        }
        ensure!(
            started.elapsed() < Duration::from_secs(10),
            "{}",
            t().system.service_resident_start_timeout
        );
        waited = true;
        // Poll readiness only: no RPC has been sent, and no side effect is replayed.
        std::thread::sleep(Duration::from_millis(20));
    }
}
pub(crate) fn paused() -> Result<bool> {
    use windows::{
        Win32::{Foundation::ERROR_FILE_NOT_FOUND, System::Registry::*},
        core::w,
    };
    let mut value = 0u32;
    let mut length = 4;
    let result = unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            w!(r"SYSTEM\CurrentControlSet\Services\FarwingHostService\Runtime"),
            w!("Paused"),
            RRF_RT_REG_DWORD,
            None,
            Some((&mut value as *mut u32).cast()),
            Some(&mut length),
        )
    };
    if result == ERROR_FILE_NOT_FOUND {
        return Ok(false);
    }
    result.ok()?;
    Ok(value != 0)
}
fn boot_pause(value: Option<bool>) -> Result<bool> {
    use windows::{Win32::System::Registry::*, core::w};
    let mut key = HKEY::default();
    unsafe {
        RegCreateKeyExW(
            HKEY_LOCAL_MACHINE,
            w!(r"SYSTEM\CurrentControlSet\Services\FarwingHostService\Runtime"),
            None,
            None,
            REG_OPTION_VOLATILE,
            KEY_QUERY_VALUE | KEY_SET_VALUE,
            None,
            &mut key,
            None,
        )
        .ok()?;
    }
    let result = (|| -> Result<bool> {
        if let Some(value) = value {
            unsafe {
                RegSetValueExW(
                    key,
                    w!("Paused"),
                    None,
                    REG_DWORD,
                    Some(&u32::from(value).to_le_bytes()),
                )
                .ok()?;
            }
            Ok(value)
        } else {
            let mut value = 0u32;
            let mut size = 4;
            let result = unsafe {
                RegGetValueW(
                    key,
                    None,
                    w!("Paused"),
                    RRF_RT_REG_DWORD,
                    None,
                    Some((&mut value as *mut u32).cast()),
                    Some(&mut size),
                )
            };
            ensure!(
                result.is_ok() || result == windows::Win32::Foundation::ERROR_FILE_NOT_FOUND,
                "{}",
                t().system.service_boot_state_read_failed
            );
            Ok(value != 0)
        }
    })();
    unsafe {
        let _ = RegCloseKey(key);
    }
    result
}
pub(crate) fn run(parent: u32) -> Result<()> {
    ensure!(
        vault::sid(std::process::id())? == "S-1-5-18",
        "无人值守进程只能由系统服务启动"
    );
    super::install::verify_running(parent)?;
    process::verify_image(parent)?;
    let parent_handle = super::pipe::Handle(unsafe {
        windows::Win32::System::Threading::OpenProcess(
            windows::Win32::System::Threading::PROCESS_SYNCHRONIZE,
            false,
            parent,
        )?
    });
    let session = process::session(std::process::id())?;
    let event: Vec<u16> = crate::product::global_object(&format!("Resident.Stop.{parent}.{session}"))
        .encode_utf16()
        .chain(Some(0))
        .collect();
    let stop_event = super::pipe::Handle(unsafe {
        windows::Win32::System::Threading::OpenEventW(
            windows::Win32::System::Threading::SYNCHRONIZATION_SYNCHRONIZE,
            false,
            windows::core::PCWSTR(event.as_ptr()),
        )?
    });
    // Handles stay owned until all scoped threads have joined. Only wait operations are shared.
    let stop_raw = stop_event.0.0 as usize;
    let parent_raw = parent_handle.0.0 as usize;
    let running = || unsafe { windows::Win32::System::Threading::WaitForSingleObject(windows::Win32::Foundation::HANDLE(stop_raw as *mut _), 0) == windows::Win32::Foundation::WAIT_TIMEOUT } && process::active_session() == session && unsafe { windows::Win32::System::Threading::WaitForSingleObject(windows::Win32::Foundation::HANDLE(parent_raw as *mut _), 0) == windows::Win32::Foundation::WAIT_TIMEOUT };
    OWNER.store(true, Ordering::Release);
    crate::features::host::displays::recovery::recover_abandoned()?;
    if let Err(error) = super::super::virtual_audio::Defaults::recover_defaults() {
        // Before Windows logon there is no user's default microphone to touch.
        // A later authorized request retries recovery before selecting ours.
        tracing::debug!(%error, "previous virtual microphone defaults not yet restored");
    }
    let runtime = tokio::runtime::Runtime::new()?;
    let (tx, rx) = tokio::sync::mpsc::channel(8);
    let stop = tokio_util::sync::CancellationToken::new();
    let alive = || running() && !stop.is_cancelled();
    std::thread::scope(|scope| -> Result<()> {
        let displays = scope.spawn(|| {
            let mut owner = crate::features::host::displays::fallback::Maintainer::default();
            let mut last = String::new();
            while alive() {
                if let Err(error) = owner.tick() {
                    let error = format!("{error:#}");
                    if error != last {
                        tracing::warn!(%error,"display maintenance deferred");
                        last = error;
                    }
                } else {
                    last.clear();
                }
                std::thread::sleep(Duration::from_secs(1));
            }
        });
        let io = scope.spawn(|| {
            let result = std::thread::scope(|workers_scope| -> Result<()> {
                let _cancel = stop.clone().drop_guard();
                let mut workers = Vec::new();
                let mut listener = Pipe::listener(&SESSION, false, 8, true)?;
                while alive() {
                    workers.retain(|w: &std::thread::ScopedJoinHandle<'_, ()>| !w.is_finished());
                    if workers.len() >= 7 {
                        std::thread::sleep(Duration::from_millis(50));
                        continue;
                    }
                    if listener.accept(&alive).is_err() {
                        continue;
                    }
                    let next = Pipe::listener(&SESSION, false, 8, false)?;
                    let pipe = std::mem::replace(&mut listener, next);
                    let tx = tx.clone();
                    let alive = &alive;
                    workers.push(workers_scope.spawn(move || {
                        let result = (|| -> Result<()> {
                            ensure!(pipe.peer_pid(true)? == parent, "后台命令不属于服务进程");
                            while alive() {
                                if !pipe.available_within(IDLE_WAIT)? {
                                    continue;
                                }
                                let request: Request =
                                    pipe.receive_timeout(Duration::from_secs(5), alive)?;
                                let (reply, result) = tokio::sync::oneshot::channel();
                                if tx.blocking_send((request, reply)).is_err() {
                                    break;
                                }
                                // Pausing drops registrations in flight; the
                                // service still waits on this pipe for an answer.
                                let reply = result.blocking_recv().unwrap_or_else(|_| {
                                    Reply::Error(
                                        t().system.service_resident_request_cancelled.into(),
                                    )
                                });
                                pipe.send(&reply, alive)?;
                            }
                            Ok(())
                        })();
                        if let Err(error) = result {
                            tracing::debug!(%error, "resident session channel closed");
                        }
                    }));
                }
                Ok(())
            });
            stop.cancel();
            result
        });
        let result = runtime.block_on(run_account(rx, &alive));
        stop.cancel();
        displays
            .join()
            .map_err(|_| anyhow::anyhow!("显示维护线程异常"))?;
        result.and(io.join().map_err(|_| anyhow::anyhow!("后台命令线程异常"))?)
    })
}
type Incoming = tokio::sync::mpsc::Receiver<(Request, tokio::sync::oneshot::Sender<Reply>)>;
async fn run_account(mut incoming: Incoming, running: &impl Fn() -> bool) -> Result<()> {
    while running() {
        if !vault::applies()? {
            let command = tokio::select! { value=incoming.recv()=>value,_=tokio::time::sleep(Duration::from_millis(200))=>None };
            if let Some((request, reply)) = command {
                let result = if matches!(
                    request,
                    Request::Pause | Request::Quiescent
                ) {
                    Reply::Done
                } else {
                    Reply::Error(t().system.service_resident_not_authorized.into())
                };
                let _ = reply.send(result);
            } else if incoming.is_closed() {
                break;
            }
            continue;
        }
        run_authorized_account(&mut incoming, running).await?;
        if incoming.is_closed() {
            break;
        }
    }
    Ok(())
}
async fn run_authorized_account(
    incoming: &mut Incoming,
    running: &impl Fn() -> bool,
) -> Result<()> {
    let device = DeviceRuntime::start()?;
    let store = KeyringSessionStore::new()?;
    let mut client: Option<HostClient> = None;
    let mut presence: Option<ActivePresence> = None;
    let mut preparation: Option<(
        tokio_util::sync::CancellationToken,
        tokio::task::JoinHandle<Result<()>>,
    )> = None;
    let mut initializers = tokio::task::JoinSet::new();
    let mut online = PresenceState::Offline;
    // Going online is deferred until media capabilities are prepared, so the
    // first incoming session does not race the encoder probe (about 10 s cold)
    // against the controller's connection deadline. Bounded by a cap.
    let mut presence_due: Option<std::time::Instant> = None;
    let mut device_changes = crate::account::device_change::relay::Relay::new();
    let mut blocked = String::new();
    let mut next_start = std::time::Instant::now();
    // Whether a signed-out host goes without a server: read every few
    // seconds, and kept when the setting cannot be read for a moment.
    let mut serverless_checked: Option<(std::time::Instant, bool)> = None;
    let mut tick = tokio::time::interval(Duration::from_millis(200));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let result: Result<()> = async {
        while running() {
            if !vault::applies()? {break;}
            if preparation.as_ref().is_some_and(|(_,task)|task.is_finished()) {
                cancel_preparation(&mut preparation).await;
            }
            let command =
                tokio::select! { value = incoming.recv() => value, _ = tick.tick() => None, _ = initializers.join_next(), if !initializers.is_empty() => None };
            if incoming.is_closed() && command.is_none() {
                break;
            }
            if let Some((request, reply)) = command {
                if paused()? && !matches!(&request,Request::Pause|Request::Quiescent) {
                    let _=reply.send(Reply::Error(t().system.service_resident_paused.into()));
                    continue;
                }
                // "Direct connections only" changed: rebuild the presence.
                let mut restart_presence = false;
                let result: Result<Reply> = match request {
                    Request::Pause => {
                        tracing::info!("background host pausing");
                        initializers.abort_all();
                        while initializers.join_next().await.is_some() {}
                        cancel_preparation(&mut preparation).await;
                        if let Some(current)=client.take(){
                            // Maintenance suspends hosting, not the account.
                            // Retiring the identity tells ActivePresence that
                            // authentication was revoked and clears saved login.
                            current.host.retire();
                            if let Some(p)=presence.take(){p.close().await;}
                            current.close().await;
                        }
                        online=PresenceState::Offline;
                        let deadline=std::time::Instant::now()+Duration::from_secs(10);
                        while !(super::activity::idle() && super::user_backend::idle().unwrap_or(false)) && std::time::Instant::now()<deadline {
                            tokio::time::sleep(Duration::from_millis(20)).await;
                        }
                        let idle = super::activity::idle() && super::user_backend::idle()?;
                        tracing::info!(idle, "background host paused");
                        if idle {Ok(Reply::Done)}else{Err(anyhow::anyhow!("{}", t().system.service_resources_not_released))}
                    },
                    Request::Quiescent => {
                        if client.as_ref().is_none_or(|c|c.host.status().connection.is_none())
                            && preparation.is_none() && initializers.is_empty() && super::activity::idle() && super::user_backend::idle()? {Ok(Reply::Done)}
                        else{Err(anyhow::anyhow!("{}", t().system.service_resident_sessions_active))}
                    },
                    Request::Reconnect => {
                        crate::session::reconnect::now();
                        Ok(Reply::Done)
                    },
                    Request::PrepareUpdate => {
                        if let Some(current)=&client {current.host.prepare_update().await.map(|_|Reply::Done)} else {Ok(Reply::Done)}
                    },
                    Request::CancelUpdate => {
                        if let Some(current)=&client {current.host.cancel_update();}
                        Ok(Reply::Done)
                    },
                    Request::Initialize { force } => {
                        let handle = device.handle();
                        initializers.spawn(async move {
                            let result = handle.ensure(force).await.map(|v| Reply::Identity(Box::new(v))).unwrap_or_else(Reply::from_error);
                            let _ = reply.send(result);
                        });
                        continue;
                    }
                    Request::Policy(value) => device
                        .handle()
                        .set_policy(value)
                        .await
                        .map(|_| Reply::Done),
                    Request::Name {
                        device: expected,
                        value,
                    } => device
                        .handle()
                        .set_name(expected, value)
                        .await
                        .map(|_| Reply::Done),
                    request => {
                        async {
                            let current = client.as_ref().context(t().system.service_account_not_restored)?;
                            let account = current.generation();
                            match request {
                                Request::Snapshot { ui, device_cursor, language } => {
                                    if let Some(language) = language.as_deref().and_then(crate::i18n::Language::from_tag) {
                                        crate::i18n::set(language);
                                    }
                                    if ui { current.host.assistance.touch_ui(); }
                                    Ok(Reply::Snapshot(Box::new(Snapshot {
                                    device_changes: Some(device_changes.read(device_cursor.as_ref())),
                                    assistance: current.host.assistance.snapshot(),
                                    publication: crate::account::reporting::snapshot(),
                                    account,
                                    online: online.clone(),
                                    settings: current.host.host_settings(),
                                    status: current.host.status(),
                                    capabilities: current.host.capabilities().map(|v| (*v).clone()),
                                    direct: crate::transport::direct::managed::HostDirect::current(),
                                    retry_in_ms: crate::session::reconnect::next().map(|at| {
                                        at.saturating_duration_since(std::time::Instant::now()).as_millis() as u64
                                    }),
                                    notices: crate::account::notices::pushes(),
                                })))},
                                Request::DirectState => Ok(Reply::DirectState {
                                    account,
                                    state: Box::new(crate::transport::direct::managed::HostDirect::current()),
                                }),
                                Request::Direct { account: expected, action } => {
                                    ensure!(expected == account, "{}", t().system.service_account_changed);
                                    let applied = crate::transport::direct::managed::apply(action)?;
                                    if let Some(device) = applied.end_sessions_of {
                                        // A device no longer trusted loses its current sessions too.
                                        crate::transport::direct::managed::end_sessions(&current.host, &device);
                                    }
                                    restart_presence = applied.restart_presence;
                                    Ok(Reply::Done)
                                }
                                Request::Assist { account: expected, action } => {
                                    ensure!(expected == account, "{}", t().system.service_account_changed);
                                    current.host.assistance.act(action)?;
                                    Ok(Reply::Done)
                                },
                                Request::RefreshWol => {ensure!(!current.is_guest(), "{}", t().common.signed_out);current.host.wol.refresh();Ok(Reply::Done)},
                                Request::WolSetup { account: expected, action } => {
                                    ensure!(expected == account && !current.is_guest(), "{}", t().system.service_account_changed_or_signed_out);
                                    current.host.wol_setup.act(action)?;
                                    Ok(Reply::Done)
                                },
                                Request::RefreshPublication => {ensure!(!current.is_guest(), "{}", t().common.signed_out);crate::account::reporting::REFRESH.notify_one();Ok(Reply::Done)},
                                Request::Settings { account: expected, settings, assistance } => {
                                    ensure!(expected == account, "{}", t().system.service_account_changed);
                                    current.host.set_assistance(assistance)?;
                                    current.host.set_host_settings(settings)?;
                                    current.host.persist_settings().await;
                                    if let Some(error) = current.host.status().settings_error {
                                        anyhow::bail!(error);
                                    }
                                    Ok(Reply::Done)
                                }
                                Request::Disconnect { account: expected, session } => {
                                    ensure!(expected == account, "{}", t().system.service_account_changed);
                                    ensure!(current.host.disconnect_session(&session), "{}", t().system.service_connection_ended);
                                    Ok(Reply::Done)
                                }
                                Request::BlockControl { account: expected, session, blocked } => {
                                    ensure!(expected == account, "{}", t().system.service_account_changed);
                                    ensure!(current.host.block_control(&session, blocked), "{}", t().system.service_connection_ended);
                                    Ok(Reply::Done)
                                }
                                Request::Retry { account: expected } => {
                                    ensure!(expected == account, "{}", t().system.service_account_changed);
                                    current.host.retry();
                                    Ok(Reply::Done)
                                }
                                Request::Retire { account: expected } => {
                                    ensure!(expected == account, "{}", t().system.service_account_changed);
                                    blocked = account;
                                    current.retire();
                                    Ok(Reply::Done)
                                }
                                _ => anyhow::bail!("{}", t().system.service_invalid_command),
                            }
                        }
                        .await
                    }
                };
                let _ = reply.send(result.unwrap_or_else(Reply::from_error));
                if restart_presence
                    && let Some(p) = presence.take()
                {
                    tracing::info!("direct-only setting changed; rebuilding the host presence");
                    p.close().await;
                    online = PresenceState::Offline;
                    presence_due = Some(std::time::Instant::now());
                }
            }
            if paused()? || !vault::applies()? { continue; }
            if client.is_none()
                && let Err(error) = crate::transport::direct::managed::take_over()
            {
                tracing::warn!(error = %format!("{error:#}"), "handed-over direct records not adopted");
            }
            let saved = store.load()?.map(|s| s.generation()).unwrap_or_default();
            let desired_identity = if saved.is_empty() {
                format!("guest:{}",device.handle().identity().client_identity()?.client_id)
            } else { saved.clone() };
            // Signed out, the service hosts as the GUI would: without a
            // server (or with direct connections only) on its own.
            let serverless = saved.is_empty() && {
                match serverless_checked {
                    Some((at, known)) if at.elapsed() < Duration::from_secs(3) => known,
                    previous => {
                        let known = HostClient::serverless_setting()
                            .or(previous.map(|(_, known)| known))
                            .unwrap_or(false);
                        serverless_checked = Some((std::time::Instant::now(), known));
                        known
                    }
                }
            };
            if client
                .as_ref()
                .is_some_and(|c| !c.is_active() || !c.matches_saved(&saved) || c.is_local() != serverless)
            {
                cancel_preparation(&mut preparation).await;
                presence_due = None;
                if let Some(current) = client.take() {
                    current.retire();
                    if let Some(p) = presence.take() {
                        p.close().await;
                    }
                    current.close().await;
                }
                online = PresenceState::Offline;
            }
            if client.is_none()
                && desired_identity != blocked
                && std::time::Instant::now() >= next_start
            {
                match if serverless { HostClient::local() } else if saved.is_empty() { HostClient::guest(device.handle()) } else { AuthenticatedClient::from_saved_session_with_device(device.handle()).map(|c| HostClient::from(Arc::new(c))) } {
                    Ok(c) => {

                        let cancel = tokio_util::sync::CancellationToken::new();
                        let owner = c.host.clone();
                        let cancelled = cancel.clone();
                        preparation = Some((
                            cancel,
                            tokio::spawn(async move { owner.prepare_startup(cancelled).await }),
                        ));
                        device_changes.reset();
                        presence_due = Some(std::time::Instant::now() + Duration::from_secs(30));
                        client = Some(c);
                    }
                    Err(error) => {
                        tracing::warn!(%error, "unattended account restoration failed");
                        next_start = std::time::Instant::now() + Duration::from_secs(3);
                    }
                }
            }
            if presence.is_none()
                && let (Some(c), Some(due)) = (&client, presence_due)
                && (preparation.as_ref().is_none_or(|(_, task)| task.is_finished())
                    || std::time::Instant::now() >= due)
            {
                presence_due = None;
                presence = Some(ActivePresence::start(c.clone()));
            }
            if let Some(p) = &presence {
                while let Ok(event) = p.events.try_recv() {
                    match event {
                        PresenceEvent::State(state) => {
                            if matches!(state, PresenceState::Online)
                                && !matches!(online, PresenceState::Online) {
                                device_changes.reset();
                            }
                            online = state;
                        },
                        PresenceEvent::DeviceChanged(change) => device_changes.push(change),
                        PresenceEvent::Warning(error) => {
                            tracing::warn!(%error, "unattended presence")
                        }
                        _ => (),
                    }
                }
            }
            if presence.as_ref().is_some_and(|p| p.task.is_finished()) {
                let result = presence
                    .take()
                    .unwrap()
                    .task
                    .await
                    .unwrap_or_else(|e| Err(e.into()));
                online = PresenceState::Offline;
                cancel_preparation(&mut preparation).await;
                if let Some(c) = client.take() {
                    if result
                        .as_ref()
                        .err()
                        .and_then(|e| e.downcast_ref::<crate::transport::signal::SignalFailure>())
                        .is_some_and(|e| {
                            matches!(e, crate::transport::signal::SignalFailure::Kicked(_))
                        })
                    {
                        blocked = c.generation();
                    }
                    c.host.retire();
                    c.close().await;
                }
                next_start =
                    std::time::Instant::now() + device.handle().startup_retry_delay().await?;
            }
        }
        Ok(())
    }
    .await;
    initializers.abort_all();
    while initializers.join_next().await.is_some() {}
    cancel_preparation(&mut preparation).await;
    if let Some(c) = client {
        c.host.retire();
        if let Some(p) = presence {
            p.close().await;
        }
        c.close().await;
    }
    device.close().await;
    result
}

async fn cancel_preparation(
    preparation: &mut Option<(
        tokio_util::sync::CancellationToken,
        tokio::task::JoinHandle<Result<()>>,
    )>,
) {
    if let Some((cancel, task)) = preparation.take() {
        cancel.cancel();
        if let Ok(Err(error)) = task.await {
            tracing::debug!(%error, "resident media preparation ended");
        }
    }
}
