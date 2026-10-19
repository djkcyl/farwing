//! One protected service boundary; physical state remains in the session agent.
use super::{
    engine::{Engine, Geometry},
    wire::Event,
};
use crate::platform::windows::host_service::{
    pipe::{NAME, Pipe},
    process,
};
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use std::time::{Duration, Instant};

#[derive(Serialize, Deserialize)]
enum Request {
    SecureAttention {
        origin: u32,
    },
    Hello {
        origin: u32,
    },
    Input {
        events: Vec<Event>,
        geometry: Geometry,
        /// The owner's held-key timeout.
        hold: Duration,
    },
    Release,
    Alive,
    Sync(Geometry),
    Configure(super::config::Configuration),
    Close,
}
#[derive(Serialize, Deserialize)]
struct Reply {
    backend: String,
    error: Option<String>,
    sas: bool,
    mouse_policy: super::config::MousePolicy,
}

/// Finish framing even when the input generation is revoked. A Release queued
/// behind the request cancels the agent's remaining transitions; drain both
/// replies before reusing the pipe. Only a real I/O failure poisons the channel.
fn exchange(pipe: &Pipe, request: &Request, permitted: impl Fn() -> bool) -> Result<Reply> {
    use std::cell::{Cell, RefCell};
    pipe.send(request, || true)?;
    let cancelled = Cell::new(false);
    let failure = RefCell::new(None);
    let poll = || {
        if !permitted() && !cancelled.replace(true) {
            if let Err(error) = pipe.send(&Request::Release, || true) {
                *failure.borrow_mut() = Some(error);
            }
        }
        failure.borrow().is_none()
    };
    ensure!(poll(), "输入取消请求发送失败");
    let reply = pipe.receive(&poll);
    let _ = poll();
    if let Some(error) = failure.into_inner() {
        return Err(error);
    }
    let mut reply: Reply = reply?;
    if cancelled.get() {
        reply = pipe.receive(|| true)?;
        reply.sas = false;
        if reply.error.is_none() {
            reply.error = Some("本次输入已取消，已释放持有状态".into());
        }
    }
    Ok(reply)
}
pub(crate) struct Remote {
    pipe: Pipe,
    backend: String,
    last: Instant,
    failed: bool,
    geometry: Option<Geometry>,
    mouse_policy: super::config::MousePolicy,
}
impl Remote {
    pub fn connect(permitted: impl Fn() -> bool) -> Result<Option<Self>> {
        let Some(pipe) = Pipe::client(&NAME)? else {
            return Ok(None);
        };
        let pid = pipe.peer_pid(false)?;
        if let Err(error) = crate::platform::windows::host_service::install::verify_running(pid) {
            if error.is::<crate::platform::windows::host_service::install::NeedsUpdate>() {
                return Ok(None);
            }
            return Err(error);
        }
        pipe.send(
            &Request::Hello {
                origin: std::process::id(),
            },
            &permitted,
        )?;
        let reply: Reply = pipe.receive_timeout(Duration::from_secs(10), &permitted)?;
        if let Some(error) = reply.error {
            anyhow::bail!(error)
        }
        Ok(Some(Self {
            pipe,
            backend: reply.backend,
            last: Instant::now(),
            failed: false,
            geometry: None,
            mouse_policy: reply.mouse_policy,
        }))
    }
    pub fn backend(&self) -> &str {
        &self.backend
    }
    pub fn healthy(&self) -> bool {
        !self.failed
    }
    fn request(&mut self, request: Request, permitted: impl Fn() -> bool) -> Result<()> {
        ensure!(!self.failed, "被控服务连接已失效");
        ensure!(permitted(), "本次输入已取消");
        let result = exchange(&self.pipe, &request, permitted);
        // A lost response does not authorize replaying the command through another
        // backend. Disconnect is the only recovery for this input generation.
        match result {
            Ok(reply) => {
                self.last = Instant::now();
                self.backend = reply.backend;
                self.mouse_policy = reply.mouse_policy;
                if let Some(error) = reply.error {
                    anyhow::bail!(error)
                }
                Ok(())
            }
            Err(error) => {
                self.failed = true;
                Err(error)
            }
        }
    }
    pub fn apply(
        &mut self,
        events: Vec<Event>,
        geometry: Geometry,
        hold: Duration,
        permitted: impl Fn() -> bool,
    ) -> Result<()> {
        self.request(
            Request::Input {
                events,
                geometry,
                hold,
            },
            permitted,
        )
    }
    pub fn release(&mut self) -> Result<()> {
        self.request(Request::Release, || true)
    }
    pub fn synchronize(&mut self, geometry: Geometry) -> Result<()> {
        if self.geometry.as_ref() != Some(&geometry) {
            self.request(Request::Sync(geometry.clone()), || true)?;
            self.geometry = Some(geometry);
        }
        Ok(())
    }
    pub fn configure(&mut self, configuration: super::config::Configuration) -> Result<()> {
        self.request(Request::Configure(configuration), || true)
    }
    pub fn mouse_policy(&self) -> super::config::MousePolicy {
        self.mouse_policy.clone()
    }
    pub fn tick(&mut self) -> Result<()> {
        if self.last.elapsed() >= Duration::from_millis(100) {
            self.request(Request::Alive, || true)?;
        }
        Ok(())
    }
}
impl Drop for Remote {
    fn drop(&mut self) {
        if !self.failed {
            let _ = self.pipe.send(&Request::Close, || true);
        }
    }
}

fn service_pid() -> Result<u32> {
    use windows::{Win32::System::Services::*, core::w};
    unsafe {
        let manager = OpenSCManagerW(None, None, SC_MANAGER_CONNECT)?;
        let service = OpenServiceW(manager, w!("FarwingHostService"), SERVICE_QUERY_STATUS);
        let _ = CloseServiceHandle(manager);
        let service = service?;
        let mut state = SERVICE_STATUS_PROCESS::default();
        let mut needed = 0;
        let bytes = std::slice::from_raw_parts_mut(
            (&mut state as *mut SERVICE_STATUS_PROCESS).cast(),
            std::mem::size_of_val(&state),
        );
        let result =
            QueryServiceStatusEx(service, SC_STATUS_PROCESS_INFO, Some(bytes), &mut needed);
        let _ = CloseServiceHandle(service);
        result?;
        ensure!(state.dwCurrentState == SERVICE_RUNNING, "被控服务未运行");
        Ok(state.dwProcessId)
    }
}

/// Runs in the active Windows session under a service-created SYSTEM token.
/// A private SYSTEM-only pipe and the launching process identity own its lifetime.
pub(crate) fn agent(name: &str, parent: u32) -> Result<()> {
    ensure!(
        name.starts_with(&crate::product::pipe_name("Input.Agent.")) && name.len() < 200,
        "输入会话管道无效"
    );
    ensure!(service_pid()? == parent, "输入会话父进程无效");
    process::verify_image(parent)?;
    let pipe = Pipe::client(name)?.ok_or_else(|| anyhow::anyhow!("输入会话已结束"))?;
    ensure!(pipe.peer_pid(false)? == parent, "输入会话服务身份无效");
    let session = process::session(std::process::id())?;
    let hardware =
        session == unsafe { windows::Win32::System::RemoteDesktop::WTSGetActiveConsoleSessionId() };
    let permitted = || {
        process::active_session() == session
            && pipe.queued_bytes().is_ok()
            && (!hardware
                || session
                    == unsafe {
                        windows::Win32::System::RemoteDesktop::WTSGetActiveConsoleSessionId()
                    })
    };
    let Request::Hello { origin } = pipe.receive(permitted)? else {
        anyhow::bail!("缺少输入会话握手")
    };
    let mut engine = Engine::new(hardware, origin, true)?;
    pipe.send(
        &Reply {
            backend: engine.backend().into(),
            error: None,
            sas: false,
            mouse_policy: engine.mouse_policy(),
        },
        permitted,
    )?;
    let mut last = Instant::now();
    while permitted() && last.elapsed() < Duration::from_millis(500) {
        if !pipe.available()? {
            engine.tick()?;
            continue;
        }
        let request: Request = pipe.receive(permitted)?;
        let close = matches!(request, Request::Close);
        let result = match request {
            Request::Input {
                events,
                geometry,
                hold,
            } => {
                ensure!(
                    !events.is_empty() && events.len() <= super::MAX_BATCH,
                    "输入批次长度无效"
                );
                for event in &events {
                    event.validate()?;
                }
                geometry.validate()?;
                engine.hold_timeout(hold);
                // The broker is request/response serialized. Extra incoming data
                // during execution is its cancellation/close request.
                let count = events.len();
                let started = Instant::now();
                let result = events.into_iter().try_for_each(|event| {
                    engine.apply(event, geometry.clone(), || {
                        permitted() && pipe.queued_bytes().is_ok_and(|n| n == 0)
                    })
                });
                if started.elapsed() >= Duration::from_millis(100) {
                    tracing::warn!(
                        events = count,
                        elapsed_ms = started.elapsed().as_millis() as u64,
                        "host input agent execution stalled"
                    );
                }
                result
            }
            Request::Release | Request::Close => engine.release(),
            Request::Alive => engine.tick(),
            Request::Sync(geometry) => {
                geometry.validate()?;
                engine.synchronize(geometry)
            }
            Request::Configure(configuration) => engine.configure(configuration),
            Request::Hello { .. } | Request::SecureAttention { .. } => {
                anyhow::bail!("输入会话请求无效")
            }
        };
        if result.is_err() {
            let _ = engine.release();
        }
        pipe.send(
            &Reply {
                backend: engine.backend().into(),
                error: result.err().map(|e| e.to_string()),
                sas: engine.take_sas(),
                mouse_policy: engine.mouse_policy(),
            },
            permitted,
        )?;
        // A slow Windows input call is not a silent/disconnected client.
        // Start the idle lease only after its serialized reply is delivered.
        last = Instant::now();
        if close {
            break;
        }
    }
    engine.release()
}

pub(crate) fn serve(pipe: Pipe, permitted: impl Fn() -> bool) -> Result<()> {
    let pid = pipe.peer_pid(true)?;
    crate::platform::windows::host_service::install::verify_client(pid)?;
    let session = process::session(pid)?;
    ensure!(
        session == process::active_session(),
        "输入请求不属于当前Windows会话"
    );
    let permitted =
        || permitted() && process::active_session() == session && pipe.queued_bytes().is_ok();
    let first = pipe.receive::<Request>(&permitted)?;
    if let Request::SecureAttention { origin } = first {
        ensure!(origin == pid, "输入来源进程不匹配");
        ensure!(
            permitted() && pipe.queued_bytes()? == 0,
            "安全注意序列请求已撤销"
        );
        let result = crate::platform::windows::host_service::service::secure_attention();
        return pipe.send(
            &Reply {
                backend: "Windows · 系统服务".into(),
                error: result.err().map(|error| error.to_string()),
                sas: false,
                mouse_policy: Default::default(),
            },
            &permitted,
        );
    }
    let Request::Hello { origin } = first else {
        anyhow::bail!("缺少被控服务握手")
    };
    ensure!(origin == pid, "输入来源进程不匹配");
    let name = crate::product::pipe_name(&format!("Input.Agent.{}", uuid::Uuid::new_v4()));
    let agent_pipe = Pipe::server(&name, false)?;
    let agent = process::Agent::start(session, &name)?;
    let started = Instant::now();
    agent_pipe
        .accept(|| permitted() && agent.alive() && started.elapsed() < Duration::from_secs(10))?;
    ensure!(
        agent_pipe.peer_pid(true)? == agent.pid,
        "输入会话进程身份无效"
    );
    agent_pipe.send(&Request::Hello { origin }, &permitted)?;
    pipe.send(&agent_pipe.receive::<Reply>(&permitted)?, &permitted)?;
    let mut last = Instant::now();
    let result = (|| -> Result<()> {
        while permitted() && agent.alive() && last.elapsed() < Duration::from_millis(500) {
            if !pipe.available()? {
                continue;
            }
            let request: Request = pipe.receive(&permitted)?;
            let close = matches!(request, Request::Close);
            let input = matches!(request, Request::Input { .. });
            let mut reply = exchange(&agent_pipe, &request, || {
                permitted() && (!input || pipe.queued_bytes().is_ok_and(|n| n == 0))
            })?;
            if reply.sas {
                reply.sas = false;
                ensure!(permitted(), "安全注意序列请求已撤销");
                if let Err(error) =
                    crate::platform::windows::host_service::service::secure_attention()
                {
                    reply.error = Some(error.to_string());
                }
            }
            pipe.send(&reply, &permitted)?;
            last = Instant::now();
            if close {
                break;
            }
        }
        Ok(())
    })();
    // Closing the app pipe cancels even an idle hold. Let the agent release before
    // the job handle provides a final crash/timeout boundary.
    let _ = agent_pipe.send(&Request::Close, || agent.alive());
    let _ = agent_pipe.receive::<Reply>(|| agent.alive());
    result
}

/// The resident already owns native input, but SAS remains an explicit service
/// operation. No second HID owner or input-agent is created for this command.
pub(super) fn secure_attention(permitted: impl Fn() -> bool) -> Result<()> {
    ensure!(permitted(), "安全注意序列请求已撤销");
    let pipe = Pipe::client(&NAME)?.ok_or_else(|| anyhow::anyhow!("被控服务未运行"))?;
    crate::platform::windows::host_service::install::verify_running(pipe.peer_pid(false)?)?;
    pipe.send(
        &Request::SecureAttention {
            origin: std::process::id(),
        },
        &permitted,
    )?;
    let reply: Reply = pipe.receive(&permitted)?;
    if let Some(error) = reply.error {
        anyhow::bail!(error);
    }
    Ok(())
}
