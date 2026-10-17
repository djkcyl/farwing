//! Single unattended account owner, independent of the ordinary desktop UI.
use super::{pipe::Pipe, process, vault};
use crate::{
    account::{
        auth::{KeyringSessionStore, SessionStore},
        client::AuthenticatedClient,
    },
    session::{
        device_session::DeviceRuntime,
        host_client::HostClient,
        presence::{ActivePresence, PresenceEvent, PresenceState},
    },
};
use anyhow::{Context, Result, ensure};
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU32, Ordering},
    },
    time::Duration,
};

const CONTROL: &str = r"\\.\pipe\OpenUUYC.Resident.Control.v1";
const SESSION: &str = r"\\.\pipe\OpenUUYC.Resident.Session.v1";
static OWNER: AtomicBool = AtomicBool::new(false);
pub(crate) fn is_owner() -> bool {
    OWNER.load(Ordering::Acquire)
}
pub(crate) fn managed() -> bool {
    !is_owner() && vault::applies().unwrap_or(false)
}

pub(crate) use crate::session::resident::{Reply, Request, Snapshot};

pub(crate) fn call(request: Request) -> Result<Reply> {
    static CONTROL_CONNECTION: std::sync::Mutex<Option<Pipe>> = std::sync::Mutex::new(None);
    static INITIALIZER_CONNECTION: std::sync::Mutex<Option<Pipe>> = std::sync::Mutex::new(None);
    // Registration can wait on the network. It must not block permission
    // revocation, disconnect or explicit exit on the management connection.
    let slot = if matches!(
        &request,
        Request::Initialize { .. } | Request::Controllable(_) | Request::Name { .. }
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
            match Pipe::client(CONTROL) {
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
                Ok(None) => anyhow::bail!("被控后台尚未启动"),
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
        .context("被控后台请求中断")?
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
            Ok(_) => anyhow::bail!("结束被控后台返回无效响应"),
            Err(error) => match super::install::running() {
                Ok(false) => Ok(()),
                _ => Err(error),
            },
        }
    })
    .await
    .context("被控后台退出请求中断")?
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
        "该 Windows 用户无权管理被控服务"
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
        let mut listener = Pipe::listener(CONTROL, true, 8, true)?;
        while running() {
            workers.retain(|worker: &std::thread::ScopedJoinHandle<'_, ()>| !worker.is_finished());
            if workers.len() >= 7 {
                std::thread::sleep(Duration::from_millis(50));
                continue;
            }
            if listener.accept(running).is_err() {
                continue;
            }
            let next = Pipe::listener(CONTROL, true, 8, false)?;
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
        if !pipe.available()? {
            continue;
        }
        let result = (|| -> Result<Reply> {
            let request: Request = pipe.receive_timeout(Duration::from_secs(5), running)?;
            match request {
                Request::FinishMigration { target } => {
                    let (_, worker) = wait_for_session(child, &AtomicBool::new(false), running)?;
                    worker.send(&Request::DeploymentReady, running)?;
                    ensure!(
                        matches!(
                            worker
                                .receive_timeout::<Reply>(Duration::from_secs(5), running)?
                                .checked()?,
                            Reply::Done
                        ),
                        "新版执行进程尚未就绪"
                    );
                    crate::platform::windows::components::migration::finish_machine(&target)
                        .map(Reply::Migration)
                }
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
                        "被控后台已退出，请重新打开程序"
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
            "后台启动等待已取消"
        );
        let pid = child.load(Ordering::Acquire);
        if pid != 0 {
            match Pipe::client(SESSION) {
                Ok(Some(pipe)) => {
                    if child.load(Ordering::Acquire) == pid {
                        ensure!(pipe.peer_pid(false)? == pid, "被控后台进程身份不匹配");
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
            "等待本机被控后台就绪超时，请检查服务状态"
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
            w!(r"SYSTEM\CurrentControlSet\Services\OpenUUYCInputService\Runtime"),
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
            w!(r"SYSTEM\CurrentControlSet\Services\OpenUUYCInputService\Runtime"),
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
                "无法读取本次开机退出状态"
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
    let event: Vec<u16> = format!("Global\\OpenUUYC.Resident.Stop.{parent}.{session}")
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
                let mut listener = Pipe::listener(SESSION, false, 8, true)?;
                while alive() {
                    workers.retain(|w: &std::thread::ScopedJoinHandle<'_, ()>| !w.is_finished());
                    if workers.len() >= 7 {
                        std::thread::sleep(Duration::from_millis(50));
                        continue;
                    }
                    if listener.accept(&alive).is_err() {
                        continue;
                    }
                    let next = Pipe::listener(SESSION, false, 8, false)?;
                    let pipe = std::mem::replace(&mut listener, next);
                    let tx = tx.clone();
                    let alive = &alive;
                    workers.push(workers_scope.spawn(move || {
                        let result = (|| -> Result<()> {
                            ensure!(pipe.peer_pid(true)? == parent, "后台命令不属于服务进程");
                            while alive() {
                                if !pipe.available()? {
                                    continue;
                                }
                                let request: Request =
                                    pipe.receive_timeout(Duration::from_secs(5), alive)?;
                                let (reply, result) = tokio::sync::oneshot::channel();
                                if tx.blocking_send((request, reply)).is_err() {
                                    break;
                                }
                                if let Ok(reply) = result.blocking_recv() {
                                    pipe.send(&reply, alive)?;
                                }
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
                    Request::Pause | Request::Quiescent | Request::DeploymentReady
                ) {
                    Reply::Done
                } else {
                    Reply::Error("本机尚未授权常驻被控".into())
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
    let mut device_changes = crate::account::device_change::relay::Relay::new();
    let mut blocked = String::new();
    let mut next_start = std::time::Instant::now();
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
                if paused()? && !matches!(&request,Request::Pause|Request::Quiescent|Request::DeploymentReady) {
                    let _=reply.send(Reply::Error("被控后台已暂停".into()));
                    continue;
                }
                let result: Result<Reply> = match request {
                    Request::DeploymentReady=>Ok(Reply::Done),
                    Request::Pause => {
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
                        if super::activity::idle() && super::user_backend::idle()? {Ok(Reply::Done)}else{Err(anyhow::anyhow!("会话资源尚未排空"))}
                    },
                    Request::Quiescent => {
                        if client.as_ref().is_none_or(|c|c.host.status().connection.is_none())
                            && preparation.is_none() && initializers.is_empty() && super::activity::idle() && super::user_backend::idle()? {Ok(Reply::Done)}
                        else{Err(anyhow::anyhow!("后台仍有活动会话"))}
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
                    Request::Controllable(value) => device
                        .handle()
                        .set_controllable(value)
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
                            let current = client.as_ref().context("后台尚未恢复账号")?;
                            let account = current.generation();
                            match request {
                                Request::Snapshot { ui, device_cursor } => {
                                    if ui { current.host.assistance.touch_ui(); }
                                    Ok(Reply::Snapshot(Box::new(Snapshot {
                                    device_changes: Some(device_changes.read(device_cursor.as_ref())),
                                    assistance: current.host.assistance.snapshot(),
                                    publication: crate::account::reporting::snapshot(),
                                    account,
                                    online: online.clone(),
                                    allowed: current.host.allowed(),
                                    encoding: current.host.encoding_settings(),
                                    audio_device: current.host.audio_device(),
                                    audio_defaults: current.host.audio_defaults(),
                                    audio_quality: current.host.audio_quality(),
                                    clipboard: current.host.clipboard_settings(),
                                    file_transfer: current.host.file_transfer_allowed(),
                                    port_mapping: current.host.port_mapping_allowed(),
                                    terminal: current.host.terminal_allowed(),
                                    terminal_local: current.host.terminal_local_allowed(),
                                    remote_power: current.host.power_allowed(),
                                    wol: current.host.wol_allowed(),
                                    status: current.host.status(),
                                    capabilities: current.host.capabilities().map(|v| (*v).clone()),
                                })))},
                                Request::Assist { account: expected, action } => {
                                    ensure!(expected == account, "账号已改变");
                                    current.host.assistance.act(action)?;
                                    Ok(Reply::Done)
                                },
                                Request::RefreshWol => {ensure!(!current.is_guest(), "请先登录");current.host.wol.refresh();Ok(Reply::Done)},
                                Request::WolSetup { account: expected, action } => {
                                    ensure!(expected == account && !current.is_guest(), "账号已改变或未登录");
                                    current.host.wol_setup.act(action)?;
                                    Ok(Reply::Done)
                                },
                                Request::RefreshPublication => {ensure!(!current.is_guest(), "请先登录");crate::account::reporting::REFRESH.notify_one();Ok(Reply::Done)},
                                Request::Settings {
                                    account: expected,
                                    allowed,
                                    encoding,
                                    audio_device,
                                    audio_defaults,
                                    audio_quality,
                                    assistance,
                                    clipboard,
                                    file_transfer,
                                    port_mapping,
                                    terminal,
                                    terminal_local,
                                    remote_power,
                                    wol,
                                } => {
                                    ensure!(expected == account, "账号已改变");
                                    encoding.validate()?;
                                    if let Some(device)=&audio_device {device.validate()?;}
                                    current.host.set_encoding_settings(encoding)?;
                                    current.host.set_audio_device(audio_device)?;
                                    current.host.set_audio_defaults(audio_defaults)?;
                                    current.host.set_audio_quality(audio_quality)?;
                                    current.host.set_assistance(assistance)?;
                                    current.host.set_clipboard_settings(clipboard)?;
                                    current.host.set_file_transfer_allowed(file_transfer)?;
                                    current.host.set_port_mapping_allowed(port_mapping)?;
                                    current.host.set_terminal_allowed(terminal)?;
                                    current.host.set_terminal_local_allowed(terminal_local)?;
                                    current.host.set_power_allowed(remote_power)?;
                                    current.host.set_wol_allowed(wol)?;
                                    current.host.set_allowed(allowed);
                                    current.host.persist_settings().await;
                                    if let Some(error) = current.host.status().settings_error {
                                        anyhow::bail!(error);
                                    }
                                    Ok(Reply::Done)
                                }
                                Request::Disconnect { account: expected, session } => {
                                    ensure!(expected == account, "账号已改变");
                                    ensure!(current.host.disconnect_session(&session), "该连接已结束或已更换");
                                    Ok(Reply::Done)
                                }
                                Request::Retry { account: expected } => {
                                    ensure!(expected == account, "账号已改变");
                                    current.host.retry();
                                    Ok(Reply::Done)
                                }
                                Request::Retire { account: expected } => {
                                    ensure!(expected == account, "账号已改变");
                                    blocked = account;
                                    current.retire();
                                    Ok(Reply::Done)
                                }
                                _ => anyhow::bail!("后台命令无效"),
                            }
                        }
                        .await
                    }
                };
                let _ = reply.send(result.unwrap_or_else(Reply::from_error));
            }
            if paused()? || !vault::applies()? { continue; }
            let saved = store.load()?.map(|s| s.generation()).unwrap_or_default();
            let desired_identity = if saved.is_empty() {
                format!("guest:{}",device.handle().identity().client_identity()?.client_id)
            } else { saved.clone() };
            if client
                .as_ref()
                .is_some_and(|c| !c.is_active() || !c.matches_saved(&saved))
            {
                cancel_preparation(&mut preparation).await;
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
                match if saved.is_empty() { HostClient::guest(device.handle()) } else { AuthenticatedClient::from_saved_session_with_device(device.handle()).map(|c| HostClient::from(Arc::new(c))) } {
                    Ok(c) => {

                        let cancel = tokio_util::sync::CancellationToken::new();
                        let owner = c.host.clone();
                        let cancelled = cancel.clone();
                        if !c.is_guest() { preparation = Some((
                            cancel,
                            tokio::spawn(async move { owner.prepare_startup(cancelled).await }),
                        ));
                        }
                        device_changes.reset();
                        presence = Some(ActivePresence::start(c.clone()));
                        client = Some(c);
                    }
                    Err(error) => {
                        tracing::warn!(%error, "unattended account restoration failed");
                        next_start = std::time::Instant::now() + Duration::from_secs(3);
                    }
                }
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
                            matches!(e, crate::transport::signal::SignalFailure::Kicked)
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
