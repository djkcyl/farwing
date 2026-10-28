//! Same protocol engine in portable mode and in an ordinary-user service child.
use super::*;
use crate::i18n::t;
use crate::platform::windows::host_service::{pipe::Pipe, process, vault, wake::Wake};
use prost::Message as _;
use std::{
    sync::{
        Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};
static PREFIX: std::sync::LazyLock<String> =
    std::sync::LazyLock::new(|| crate::product::pipe_name("Files."));
/// Frames per direction in one exchange with the executor.
const BATCH: usize = 16;
/// Bytes of outgoing frames per exchange.
const BATCH_BYTES: usize = 1024 * 1024;
/// How long an idle worker sleeps between looks at its session and policy;
/// queued frames wake it at once.
const IDLE: Duration = Duration::from_millis(100);
#[derive(prost::Message)]
struct Request {
    #[prost(message, optional, tag = "6")]
    capabilities: Option<Capabilities>,
    #[prost(uint64, tag = "5")]
    revision: u64,
    #[prost(string, tag = "1")]
    scope: String,
    #[prost(bool, tag = "2")]
    enabled: bool,
    #[prost(bytes, repeated, tag = "3")]
    packets: Vec<Vec<u8>>,
    #[prost(bool, tag = "4")]
    close: bool,
}
#[derive(prost::Message)]
struct Reply {
    #[prost(message, repeated, tag = "2")]
    notices: Vec<notices::Notice>,
    #[prost(message, repeated, tag = "1")]
    packets: Vec<Packet>,
    /// The agent's readiness event, in its first reply.
    #[prost(string, optional, tag = "3")]
    wake: Option<String>,
}
pub(crate) struct Backend {
    pub notices: notices::Journal,
    pub input: mpsc::Sender<(u64, Vec<u8>)>,
    policy: Arc<Mutex<(u64, bool, Capabilities)>>,
    pub ready: Arc<AtomicBool>,
    pub error: Arc<Mutex<Option<String>>>,
    pub output: mpsc::Receiver<(u64, Packet)>,
    wake: Arc<Wake>,
    worker: tokio::task::JoinHandle<()>,
    stop: CancellationToken,
}
impl Backend {
    pub fn new(scope: String, stop: CancellationToken) -> Result<Self> {
        let wake = Arc::new(Wake::new()?);
        let worker_wake = wake.clone();
        let (input, rx) = mpsc::channel(32);
        let (tx, output) = mpsc::channel(32);
        let policy = Arc::new(Mutex::new((0, false, Capabilities::default())));
        let settings = policy.clone();
        let ready = Arc::new(AtomicBool::new(false));
        let available = ready.clone();
        let error = Arc::new(Mutex::new(None));
        let status = error.clone();
        let cancel = stop.clone();
        let notices = notices::Journal::default();
        let updates = notices.clone();
        let activity = crate::platform::windows::host_service::activity::Work::new();
        let worker = tokio::task::spawn_blocking(move || {
            let _activity = activity;
            if let Err(e) = worker(scope, rx, tx, settings, available, cancel, updates, worker_wake) {
                tracing::warn!(error=%format!("{e:#}"), "host file executor stopped");
                *super::super::lock(&status) = Some(e.to_string());
            }
        });
        Ok(Self {
            notices,
            input,
            policy,
            ready,
            error,
            output,
            wake,
            worker,
            stop,
        })
    }
    pub fn policy(&self, enabled: bool, capabilities: Capabilities) -> u64 {
        let mut state = super::super::lock(&self.policy);
        let changed = state.1 != enabled || state.2 != capabilities;
        if state.1 != enabled {
            state.0 = state.0.wrapping_add(1);
            state.1 = enabled;
        }
        state.2 = capabilities;
        if changed {
            self.wake.notify();
        }
        state.0
    }
    /// A frame was queued on `input`.
    pub fn notify(&self) {
        self.wake.notify();
    }
    pub async fn close(self) -> Vec<notices::Notice> {
        self.stop.cancel();
        let _ = self.worker.await;
        self.notices.take()
    }
}
struct Local {
    journal: notices::Journal,
    scope: String,
    revision: u64,
    engine: Option<Engine>,
    /// Moves the engine's frames to `rx` and wakes the worker.
    forward: Option<tokio::task::JoinHandle<()>>,
    wake: Arc<Wake>,
    tx: mpsc::Sender<Packet>,
    rx: mpsc::Receiver<Packet>,
    runtime: tokio::runtime::Runtime,
}
impl Local {
    fn new(scope: String, wake: Arc<Wake>) -> Result<Self> {
        let (tx, rx) = mpsc::channel(32);
        Ok(Self {
            journal: Default::default(),
            scope,
            revision: 0,
            engine: None,
            forward: None,
            wake,
            tx,
            rx,
            runtime: tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .thread_name("file-transfer-user")
                .build()?,
        })
    }
    fn exchange(&mut self, request: Request) -> Result<Reply> {
        ensure!(
            request.scope == self.scope && request.packets.len() <= BATCH,
            "{}",
            t().features.transfer_agent_request_invalid
        );
        if (!request.enabled || request.close || request.revision != self.revision)
            && self.engine.is_some()
        {
            let engine = self.engine.take().unwrap();
            let denied = if request.close {
                Vec::new()
            } else {
                engine.revoked()
            };
            self.runtime.block_on(engine.close());
            // Nothing of the closed engine may follow.
            if let Some(forward) = self.forward.take() {
                forward.abort();
                let _ = self.runtime.block_on(forward);
            }
            while self.rx.try_recv().is_ok() {}
            for packet in denied {
                self.tx.try_send(packet)?;
            }
        }
        self.revision = request.revision;
        if request.enabled && !request.close && self.engine.is_none() {
            let (output, mut frames) = mpsc::channel(32);
            let (tx, wake) = (self.tx.clone(), self.wake.clone());
            self.forward = Some(self.runtime.spawn(async move {
                while let Some(packet) = frames.recv().await {
                    if tx.send(packet).await.is_err() {
                        break;
                    }
                    wake.notify();
                }
            }));
            self.engine = Some(Engine::new(&self.scope, output, self.journal.clone())?);
        }
        let mut packets = Vec::new();
        for packet in request.packets {
            ensure!(packet.len() <= MAX_FRAME, "{}", t().features.transfer_agent_message_too_large);
            let rejected = if let Some(engine) = &mut self.engine {
                match self.runtime.block_on(engine.receive(&packet)) {
                    Ok(()) => None,
                    Err(error) => {
                        tracing::warn!(error = %format!("{error:#}"), "host file request rejected");
                        Some(failure(&error))
                    }
                }
            } else {
                Some(Failure::new(code::DENIED, t().features.transfer_permission_off))
            };
            if let Some(failure) = rejected {
                // A business rejection belongs to this request, not the user
                // executor. Replies have bounded space independent of a busy
                // transfer's output queue; unrelated tasks continue running.
                packets.extend(reject(&packet, &failure));
            }
        }
        let mut bytes = packets.iter().map(|p| p.data.len()).sum::<usize>();
        while packets.len() < BATCH && bytes < BATCH_BYTES {
            match self.rx.try_recv() {
                Ok(p) => {
                    bytes += p.data.len();
                    packets.push(p);
                }
                Err(_) => break,
            }
        }
        Ok(Reply {
            packets,
            notices: self.journal.take(),
            wake: None,
        })
    }
}
impl Drop for Local {
    fn drop(&mut self) {
        if let Some(engine) = self.engine.take() {
            self.runtime.block_on(engine.close());
        }
    }
}

struct Remote {
    pipe: Pipe,
    agent: crate::platform::windows::host_service::user_backend::Lease,
    session: u32,
    /// Signalled by the agent when it has frames.
    wake: Option<Wake>,
}
impl Remote {
    fn new(stop: &CancellationToken) -> Result<Self> {
        let session = process::active_session();
        let name = format!("{}{}", *PREFIX, uuid::Uuid::new_v4().simple());
        let pipe = Pipe::server(&name, true)?;
        let agent = crate::platform::windows::host_service::user_backend::Lease::connect(
            crate::platform::windows::host_service::user_backend::Role::Files,
            &name,
            session,
            || !stop.is_cancelled(),
        )?;
        let until = Instant::now() + Duration::from_secs(10);
        pipe.accept(|| !stop.is_cancelled() && agent.alive() && Instant::now() < until)?;
        ensure!(
            pipe.peer_pid(true)? == agent.pid && pipe.peer_session(true)? == session,
            "文件用户进程身份不符"
        );
        Ok(Self {
            pipe,
            agent,
            session,
            wake: None,
        })
    }
    fn exchange(&mut self, request: &Request, stop: &CancellationToken) -> Result<Reply> {
        let allowed = || {
            !stop.is_cancelled() && self.agent.alive() && process::active_session() == self.session
        };
        self.pipe.send_raw(request.encode_to_vec(), allowed)?;
        let reply = Reply::decode(
            self.pipe
                .receive_raw(Duration::from_secs(10), allowed)?
                .as_slice(),
        )?;
        if self.wake.is_none()
            && let Some(name) = &reply.wake
        {
            match Wake::open(name) {
                Ok(wake) => self.wake = Some(wake),
                Err(error) => tracing::warn!(%error, "file agent event unavailable; polling it"),
            }
        }
        Ok(reply)
    }
}
fn worker(
    scope: String,
    mut input: mpsc::Receiver<(u64, Vec<u8>)>,
    output: mpsc::Sender<(u64, Packet)>,
    policy: Arc<Mutex<(u64, bool, Capabilities)>>,
    ready: Arc<AtomicBool>,
    stop: CancellationToken,
    updates: notices::Journal,
    wake: Arc<Wake>,
) -> Result<()> {
    let system = vault::sid(std::process::id())? == "S-1-5-18";
    let mut local = if system {
        None
    } else {
        Some(Local::new(scope.clone(), wake.clone())?)
    };
    let mut remote: Option<Remote> = None;
    let mut retry = Instant::now();
    // Busy while frames move in either direction; asleep until something is
    // queued when idle (input, policy and engine output all signal).
    let mut idle = false;
    let result = (|| -> Result<()> {
        while !stop.is_cancelled() {
            let mut packets = Vec::new();
            match input.try_recv() {
                Ok(p) => packets.push(p),
                Err(mpsc::error::TryRecvError::Disconnected) => break,
                Err(_) if idle => {
                    let remote_wake = remote.as_ref().and_then(|r| r.wake.as_ref());
                    let timeout = if remote.is_some() && remote_wake.is_none() {
                        Duration::from_millis(5)
                    } else {
                        IDLE
                    };
                    wake.wait(remote_wake, timeout)?;
                    if let Ok(p) = input.try_recv() {
                        packets.push(p);
                    }
                }
                Err(_) => {}
            }
            for _ in 1..BATCH {
                match input.try_recv() {
                    Ok(p) => packets.push(p),
                    Err(_) => break,
                }
            }
            let (revision, requested, capabilities) = *super::super::lock(&policy);
            let packets = packets
                .into_iter()
                .filter(|(g, _)| *g == revision)
                .map(|(_, p)| p)
                .collect();
            if remote
                .as_ref()
                .is_some_and(|r| r.session != process::active_session())
            {
                anyhow::bail!("{}", t().features.transfer_user_session_switched);
            }
            if system && remote.is_none() && requested && Instant::now() >= retry {
                match Remote::new(&stop) {
                    Ok(r) => remote = Some(r),
                    Err(_) => retry = Instant::now() + Duration::from_secs(1),
                }
            }
            ready.store(local.is_some() || remote.is_some(), Ordering::Release);
            let request = Request {
                capabilities: Some(capabilities),
                revision,
                scope: scope.clone(),
                enabled: requested,
                packets,
                close: false,
            };
            let received = !request.packets.is_empty();
            let reply = if let Some(local) = &mut local {
                local.exchange(request)?
            } else if let Some(remote) = &mut remote {
                remote.exchange(&request, &stop)?
            } else {
                let failure = if requested {
                    Failure::new(code::BUSY, t().features.transfer_no_signed_in_user)
                } else {
                    Failure::new(code::DENIED, t().features.transfer_permission_off)
                };
                Reply {
                    notices: Vec::new(),
                    wake: None,
                    packets: request
                        .packets
                        .iter()
                        .flat_map(|p| reject(p, &failure))
                        .collect(),
                }
            };
            idle = !received && reply.packets.is_empty();
            for notice in reply.notices {
                updates.publish(notice);
            }
            for packet in reply.packets {
                let mut packet = (revision, packet);
                loop {
                    if stop.is_cancelled() {
                        return Ok(());
                    }
                    match output.try_send(packet) {
                        Ok(()) => break,
                        Err(mpsc::error::TrySendError::Full(p)) => {
                            packet = p;
                            std::thread::sleep(Duration::from_millis(1));
                        }
                        Err(_) => return Ok(()),
                    }
                }
            }
        }
        Ok(())
    })();
    if let Some(mut remote) = remote {
        let close = Request {
            capabilities: None,
            revision: 0,
            scope,
            enabled: false,
            packets: Vec::new(),
            close: true,
        };
        // A cancelled exchange may have written only a frame prefix, or read
        // only part of a reply. Never append another request to that stream.
        // It would become the missing payload of the previous frame and leave
        // the user backend waiting for its ten-second receive timeout.
        if result.is_ok() {
            if let Ok(reply) = remote.exchange(&close, &CancellationToken::new()) {
                for notice in reply.notices {
                    updates.publish(notice);
                }
            }
        }
        let Remote { pipe, agent, .. } = remote;
        // EOF cancels the private job even when the close exchange failed.
        // Its separate control pipe still confirms real resource teardown.
        drop(pipe);
        if let Err(error) = agent.finish() {
            tracing::debug!(%error, "file user job ended without a clean finish reply");
        }
    }
    if let Some(local) = &mut local {
        if let Some(engine) = local.engine.take() {
            local.runtime.block_on(engine.close());
        }
        for notice in local.journal.take() {
            updates.publish(notice);
        }
    }
    drop(local);
    if stop.is_cancelled() { Ok(()) } else { result }
}
pub(crate) fn run(name: &str, parent: u32) -> Result<()> {
    ensure!(
        name.starts_with(PREFIX.as_str()) && name.len() < 150,
        "文件代理管道无效"
    );
    ensure!(
        vault::sid(std::process::id())? != "S-1-5-18",
        "文件执行不能使用SYSTEM身份"
    );
    let pipe = Pipe::client(name)?.context("文件代理连接已结束")?;
    let session = process::session(std::process::id())?;
    ensure!(
        pipe.peer_pid(false)? == parent && pipe.peer_session(false)? == session,
        "文件代理父进程不符"
    );
    let permitted = || {
        crate::platform::windows::host_service::user_backend::permitted()
            && process::active_session() == session
            && pipe.queued_bytes().is_ok()
    };
    let mut local: Option<Local> = None;
    // The parent sleeps until this is signalled (or polls without it).
    let (wake, mut wake_name) = match Wake::named() {
        Ok((wake, name)) => (Arc::new(wake), Some(name)),
        Err(error) => {
            tracing::warn!(%error, "file agent event unavailable; the parent polls");
            (Arc::new(Wake::new()?), None)
        }
    };
    while permitted() {
        let request = Request::decode(
            pipe.receive_raw(Duration::from_secs(10), permitted)?
                .as_slice(),
        )?;
        let close = request.close;
        if local.is_none() {
            local = Some(Local::new(request.scope.clone(), wake.clone())?);
        }
        let mut reply = local.as_mut().unwrap().exchange(request)?;
        reply.wake = wake_name.take();
        pipe.send_raw(reply.encode_to_vec(), permitted)?;
        if close {
            break;
        }
    }
    Ok(())
}
