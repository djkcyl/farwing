//! Ordinary-user clipboard execution for the SYSTEM resident. Only bounded protocol frames cross IPC.
use super::wake::Wake;
use super::{Clipboard, Settings, Status};
use crate::features::drag_drop::{Endpoint, HostPolicy, Role};
use crate::i18n::t;
use crate::platform::windows::host_service::{pipe::Pipe, process, vault};
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use std::{
    sync::{Arc, Mutex, mpsc},
    time::Duration,
};
use tokio_util::sync::CancellationToken;

static PREFIX: std::sync::LazyLock<String> =
    std::sync::LazyLock::new(|| crate::product::pipe_name("Clipboard."));
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Policy {
    revision: u64,
    /// Automatic sync.
    settings: Settings,
    /// Manual `Pull` by this participant.
    pull: Settings,
    drag: HostPolicy,
}
impl Default for Policy {
    fn default() -> Self {
        let off = Settings {
            enabled: false,
            files: false,
        };
        Self {
            revision: 0,
            drag: HostPolicy::default(),
            settings: off,
            pull: off,
        }
    }
}
/// Whether a frame belongs to the native drag endpoint rather than the
/// clipboard exchange.
fn is_drag(data: &[u8]) -> bool {
    crate::protocol::drag_drop::is_packet(data)
        || crate::transport::transfer::kind_of(data) == Some(crate::transport::transfer::kind::DRAG)
}
#[derive(Serialize, Deserialize)]
pub(super) struct Request {
    policy: Policy,
    pub(super) packet: Option<Vec<u8>>,
    close: bool,
}
#[derive(Serialize, Deserialize)]
pub(super) struct Reply {
    status: Status,
    pub(super) packet: Option<Outbound>,
    #[serde(default)]
    wake: Option<String>,
}

#[derive(Serialize, Deserialize)]
pub(super) struct Outbound {
    pub revision: u64,
    pub native: bool,
    pub bulk: bool,
    pub data: Vec<u8>,
}
fn current(policy: &Policy, packet: &Outbound) -> bool {
    if !packet.native {
        return packet.revision == policy.revision;
    }
    if packet.revision != policy.drag.token {
        return false;
    }
    // While disabled only drag teardown passes; file data (raw `TRANSFER`
    // frames are not drag packets) and file lists are held back.
    policy.drag.enabled
        || crate::protocol::drag_drop::decode(&packet.data)
            .ok()
            .flatten()
            .is_some_and(|p| {
                !matches!(
                    p.payload,
                    Some(
                        crate::protocol::drag_drop::Payload::Files(_)
                            | crate::protocol::drag_drop::Payload::Begin(_)
                    )
                )
            })
}
pub(super) struct Backend {
    wake: Arc<Wake>,
    policy: Arc<Mutex<Policy>>,
    status: Arc<Mutex<Status>>,
    input: mpsc::SyncSender<Outbound>,
    output: tokio::sync::Mutex<tokio::sync::mpsc::Receiver<Outbound>>,
    worker: tokio::task::JoinHandle<()>,
}
impl Backend {
    pub async fn new(stop: CancellationToken) -> Result<Self> {
        let policy = Arc::new(Mutex::new(Policy::default()));
        let status = Arc::new(Mutex::new(Status::default()));
        let (input, incoming) = mpsc::sync_channel(super::INBOX);
        let (outgoing, output) = tokio::sync::mpsc::channel(32);
        let settings = policy.clone();
        let wake = Arc::new(Wake::new()?);
        let worker_wake = wake.clone();
        let reported = status.clone();
        let activity = crate::platform::windows::host_service::activity::Work::new();
        let worker = tokio::task::spawn_blocking(move || {
            let _activity = activity;
            let result = worker(
                settings,
                reported.clone(),
                incoming,
                outgoing,
                stop,
                worker_wake,
            );
            if let Err(error) = result {
                *super::super::lock(&reported) = Status {
                    error: Some(error.to_string()),
                    ..Default::default()
                };
            }
        });
        Ok(Self {
            wake,
            policy,
            status,
            input,
            output: tokio::sync::Mutex::new(output),
            worker,
        })
    }
    pub fn policy(&self, settings: Settings, pull: Settings, drag: HostPolicy) {
        let mut p = super::super::lock(&self.policy);
        let changed = p.drag != drag || p.settings != settings || p.pull != pull;
        p.drag = drag;
        if p.settings != settings || p.pull != pull {
            p.revision = p.revision.wrapping_add(1);
            p.settings = settings;
            p.pull = pull;
        }
        if changed {
            self.wake.notify();
        }
    }
    pub fn status(&self) -> Status {
        super::super::lock(&self.status).clone()
    }
    pub fn receive(&self, data: Vec<u8>) -> Result<()> {
        let policy = super::super::lock(&self.policy);
        let native = is_drag(&data);
        let revision = if native {
            policy.drag.token
        } else {
            policy.revision
        };
        self.input
            .try_send(Outbound {
                revision,
                native,
                bulk: false,
                data,
            })
            .map_err(|_| anyhow::anyhow!("{}", t().features.clipboard_ole_queue_full))?;
        self.wake.notify();
        Ok(())
    }
    pub async fn next(&self) -> Option<Outbound> {
        let mut output = self.output.lock().await;
        while let Some(packet) = output.recv().await {
            self.wake.notify();
            if current(&super::super::lock(&self.policy), &packet) {
                return Some(packet);
            }
        }
        None
    }
    pub async fn close(self) {
        let _ = self.worker.await;
    }
}

struct Drag {
    endpoint: Endpoint,
    output: tokio::sync::mpsc::Receiver<crate::features::drag_drop::Outgoing>,
}
struct Local {
    wake: Arc<Wake>,
    clipboard: Clipboard,
    drag: std::cell::RefCell<Option<Drag>>,
    prefer_drag: std::cell::Cell<bool>,
    policy: Arc<Mutex<Policy>>,
    output: mpsc::Receiver<(u64, u64, Vec<u8>)>,
    _runtime: tokio::runtime::Runtime,
    sender: tokio::task::JoinHandle<()>,
    checked: std::cell::Cell<Option<std::time::Instant>>,
}
impl Local {
    fn new(wake: Arc<Wake>) -> Result<Self> {
        Self::with_clipboard(
            wake,
            Clipboard::guarded(Some(Arc::new(desktop_available))),
        )
    }
    fn with_clipboard(
        wake: Arc<Wake>,
        clipboard: Clipboard,
    ) -> Result<Self> {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .thread_name("clipboard-send")
            .build()?;
        let policy = Arc::new(Mutex::new(Policy::default()));
        let state = policy.clone();
        let (send, output) = mpsc::sync_channel(256);
        let sent = wake.clone();
        let future = clipboard.sender(move |epoch, packet| {
            let revision = super::super::lock(&state).revision;
            let send = send.clone();
            let sent = sent.clone();
            async move {
                // Stream chunks are produced faster than the worker forwards
                // them; wait for room instead of failing.
                let mut item = (revision, epoch, packet);
                loop {
                    match send.try_send(item) {
                        Ok(()) => break,
                        Err(mpsc::TrySendError::Full(value)) => {
                            item = value;
                            sent.notify();
                            tokio::time::sleep(Duration::from_millis(1)).await;
                        }
                        Err(mpsc::TrySendError::Disconnected(_)) => {
                            anyhow::bail!("{}", t().features.clipboard_send_queue_closed)
                        }
                    }
                }
                sent.notify();
                Ok(())
            }
        });
        let sender = runtime.spawn(future);
        Ok(Self {
            wake,
            clipboard,
            drag: Default::default(),
            prefer_drag: std::cell::Cell::new(false),
            policy,
            output,
            _runtime: runtime,
            sender,
            checked: std::cell::Cell::new(None),
        })
    }
    fn update(&self, policy: Policy) -> Result<()> {
        let old = super::super::lock(&self.policy).clone();
        let changed = old.revision != policy.revision
            || old.settings != policy.settings
            || old.pull != policy.pull;
        let drag_changed = old.drag != policy.drag;
        if !changed
            && !drag_changed
            && self
                .checked
                .get()
                .is_some_and(|t| t.elapsed() < Duration::from_millis(100))
        {
            return Ok(());
        }
        self.checked.set(Some(std::time::Instant::now()));
        if changed {
            self.clipboard.suspend();
        }
        *super::super::lock(&self.policy) = policy.clone();
        let desktop = desktop_available();
        if policy.drag.token != 0 && self.drag.borrow().is_none() {
            let _runtime = self._runtime.enter();
            let (endpoint, output) = Endpoint::new(Role::Host);
            let wake = self.wake.clone();
            endpoint.on_output(Arc::new(move || wake.notify()));
            *self.drag.borrow_mut() = Some(Drag { endpoint, output });
        }
        if let Some(drag) = self.drag.borrow().as_ref() {
            drag.endpoint.host_policy(
                policy.drag.token,
                policy.drag.enabled && desktop,
                policy.drag.screens.clone(),
            );
        }
        // The exchange runs for automatic sync or manual pull; only automatic
        // sync offers local copies and applies the participant's copies.
        // Do not initialize OLE or enumerate existing private contents while inactive.
        let ready = (policy.settings.enabled || policy.pull.enabled) && desktop;
        let auto = policy.settings.enabled && desktop;
        let files = (policy.settings.enabled && policy.settings.files)
            || (policy.pull.enabled && policy.pull.files);
        self.clipboard.host_role();
        self.clipboard.set_enabled(true)?;
        self.clipboard.set_files(true);
        self.clipboard.policy(ready, auto, files);
        Ok(())
    }
    fn receive(&self, packet: &[u8]) -> Result<()> {
        if is_drag(packet) {
            if let Some(drag) = self.drag.borrow().as_ref() {
                if let Err(error) = drag.endpoint.receive(packet) {
                    tracing::warn!(%error,"native drag receiver disabled");
                    drag.endpoint.enable(false);
                }
            }
        } else {
            self.clipboard.receive(packet)?;
        }
        Ok(())
    }
    fn status(&self) -> Status {
        let mut status = super::snapshot(&self.clipboard);
        status.dragging |= self
            .drag
            .borrow()
            .as_ref()
            .is_some_and(|d| d.endpoint.pointer_owned());
        status
    }
    fn packet(&self) -> Option<Outbound> {
        let policy = super::super::lock(&self.policy).clone();
        let native = || {
            let mut drag = self.drag.borrow_mut();
            let drag = drag.as_mut()?;
            while let Ok(packet) = drag.output.try_recv() {
                let packet = Outbound {
                    revision: packet.token,
                    native: true,
                    bulk: packet.bulk,
                    data: packet.data,
                };
                if current(&policy, &packet) {
                    return Some(packet);
                }
            }
            None
        };
        let normal = || {
            while let Ok((revision, epoch, data)) = self.output.try_recv() {
                if revision == policy.revision && self.clipboard.delivery_allowed(epoch) {
                    return Some(Outbound {
                        revision,
                        native: false,
                        bulk: crate::features::clipboard::is_bulk(&data),
                        data,
                    });
                }
            }
            None
        };
        let preferred = !self.prefer_drag.replace(!self.prefer_drag.get());
        if preferred {
            native().or_else(normal)
        } else {
            normal().or_else(native)
        }
    }
}
impl Drop for Local {
    fn drop(&mut self) {
        self.clipboard.suspend();
        if let Some(drag) = self.drag.borrow_mut().take() {
            drag.endpoint.close();
        }
        self.sender.abort();
    }
}

fn worker(
    policy: Arc<Mutex<Policy>>,
    status: Arc<Mutex<Status>>,
    incoming: mpsc::Receiver<Outbound>,
    outgoing: tokio::sync::mpsc::Sender<Outbound>,
    stop: CancellationToken,
    wake: Arc<Wake>,
) -> Result<()> {
    let system = vault::sid(std::process::id())? == "S-1-5-18";
    let local = if system {
        None
    } else {
        Some(Local::new(wake.clone())?)
    };
    let mut remote = None;
    let result = (|| -> Result<()> {
        while !stop.is_cancelled() {
            let pending = match incoming.try_recv() {
                Ok(v) => Some(v),
                Err(mpsc::TryRecvError::Empty) => None,
                Err(mpsc::TryRecvError::Disconnected) => break,
            };
            let had_input = pending.is_some();
            let p = super::super::lock(&policy).clone();
            let packet = pending
                .filter(|packet| {
                    packet.revision
                        == if packet.native {
                            p.drag.token
                        } else {
                            p.revision
                        }
                })
                .map(|packet| packet.data);
            let reply = if let Some(local) = &local {
                local.update(p.clone())?;
                if let Some(packet) = packet {
                    local.receive(&packet)?;
                }
                Reply {
                    wake: None,
                    packet: local.packet(),
                    status: local.status(),
                }
            } else {
                // There is no interactive user before logon. Keep video/input alive, without granting SYSTEM clipboard access.
                if remote.is_none()
                    && ((!p.settings.enabled && !p.pull.enabled && !p.drag.enabled)
                        || !desktop_available())
                {
                    Reply {
                        wake: None,
                        status: Status::default(),
                        packet: packet
                            .as_deref()
                            .filter(|p| !is_drag(p))
                            .map(crate::features::clipboard::reject_request)
                            .transpose()?
                            .flatten()
                            .map(|data| Outbound {
                                revision: p.revision,
                                native: false,
                                bulk: false,
                                data,
                            }),
                    }
                } else {
                    if remote.is_none() {
                        remote = Some(Remote::new(&stop)?);
                    }
                    remote.as_mut().unwrap().exchange(
                        Request {
                            policy: p,
                            packet,
                            close: false,
                        },
                        &stop,
                    )?
                }
            };
            *super::super::lock(&status) = reply.status;
            let had_output = reply.packet.is_some();
            if let Some(packet) = reply.packet {
                let mut packet = packet;
                loop {
                    if stop.is_cancelled() {
                        break;
                    }
                    match outgoing.try_send(packet) {
                        Ok(()) => break,
                        Err(tokio::sync::mpsc::error::TrySendError::Full(value)) => {
                            packet = value;
                            wake.wait(None)?;
                        }
                        Err(_) => anyhow::bail!("{}", t().features.clipboard_network_send_closed),
                    }
                }
            }
            if !had_input && !had_output {
                wake.wait(remote.as_ref().and_then(|r| r.wake.as_ref()))?;
            }
        }
        Ok(())
    })();
    if let Some(remote) = remote {
        if result.is_ok() {
            let _ = super::frame::send(
                &remote.pipe,
                Request {
                    policy: Policy::default(),
                    packet: None,
                    close: true,
                },
                || true,
            );
        }
        // An incomplete frame cannot be reused for a close request. Close the
        // data endpoint before waiting for the independent job-finished reply.
        let Remote { pipe, agent, .. } = remote;
        drop(pipe);
        // Wait for OLE cancellation/retention before closing the job.
        let _ = agent.finish();
    }
    if stop.is_cancelled() { Ok(()) } else { result }
}

struct Remote {
    pipe: Pipe,
    wake: Option<Wake>,
    agent: crate::platform::windows::host_service::user_backend::Lease,
}
impl Remote {
    fn new(stop: &CancellationToken) -> Result<Self> {
        let name = format!("{}{}", *PREFIX, uuid::Uuid::new_v4().simple());
        let pipe = Pipe::server(&name, true)?;
        let agent = crate::platform::windows::host_service::user_backend::Lease::connect(
            crate::platform::windows::host_service::user_backend::Role::Clipboard,
            &name,
            process::active_session(),
            || !stop.is_cancelled(),
        )?;
        let until = std::time::Instant::now() + Duration::from_secs(10);
        pipe.accept(|| !stop.is_cancelled() && agent.alive() && std::time::Instant::now() < until)?;
        ensure!(
            pipe.peer_pid(true)? == agent.pid,
            "{}",
            t().features.clipboard_agent_pid_mismatch
        );
        ensure!(
            pipe.peer_session(true)? == process::active_session(),
            "{}",
            t().features.clipboard_agent_session_changed
        );
        Ok(Self {
            pipe,
            agent,
            wake: None,
        })
    }
    fn exchange(&mut self, request: Request, _stop: &CancellationToken) -> Result<Reply> {
        let permitted = || self.agent.alive();
        super::frame::send(&self.pipe, request, permitted)?;
        let reply: Reply = super::frame::receive(&self.pipe, Duration::from_secs(2), permitted)?;
        if self.wake.is_none()
            && let Some(name) = &reply.wake
        {
            match Wake::open(name) {
                Ok(wake) => self.wake = Some(wake),
                Err(error) => {
                    tracing::warn!(%error, "clipboard queue event unavailable; retaining idle polling")
                }
            }
        }
        Ok(reply)
    }
}

pub(crate) fn run(name: &str, parent: u32) -> Result<()> {
    ensure!(
        name.starts_with(PREFIX.as_str()) && name.len() < 150,
        "{}",
        t().features.clipboard_pipe_invalid
    );
    ensure!(
        vault::sid(std::process::id())? != "S-1-5-18",
        "{}",
        t().features.clipboard_system_account
    );
    let pipe = Pipe::client(name)?.ok_or_else(|| anyhow::anyhow!("{}", t().features.clipboard_session_ended))?;
    ensure!(pipe.peer_pid(false)? == parent, "{}", t().features.clipboard_parent_mismatch);
    let session = process::session(std::process::id())?;
    // A normal user cannot OpenProcess on the SYSTEM resident. Windows supplies
    // the authenticated pipe peer identity without granting process/token access.
    // The privileged parent independently accepts only its own newly spawned PID.
    ensure!(pipe.peer_session(false)? == session, "{}", t().features.clipboard_session_mismatch);
    let permitted = || {
        crate::platform::windows::host_service::user_backend::permitted()
            && process::active_session() == session
            && pipe.queued_bytes().is_ok()
    };
    let mut local = None;
    let (wake, mut wake_name) = match Wake::named() {
        Ok((wake, name)) => (Arc::new(wake), Some(name)),
        Err(error) => {
            tracing::warn!(%error, "clipboard queue event unavailable; retaining idle polling");
            (Arc::new(Wake::new()?), None)
        }
    };
    let result = (|| -> Result<()> {
        while permitted() {
            let request: Request = super::frame::receive(&pipe, Duration::from_secs(3), permitted)?;
            if request.close {
                break;
            }
            if local.is_none() {
                local = Some(Local::new(wake.clone())?);
            }
            let local = local.as_ref().unwrap();
            local.update(request.policy)?;
            if let Some(packet) = request.packet {
                ensure!(packet.len() < 524288, "{}", t().features.clipboard_ipc_too_large);
                local.receive(&packet)?;
            }
            super::frame::send(
                &pipe,
                Reply {
                    wake: wake_name.take(),
                    packet: local.packet(),
                    status: local.status(),
                },
                permitted,
            )?;
        }
        Ok(())
    })();
    drop(local);
    if !crate::platform::windows::host_service::user_backend::active() {
        crate::features::clipboard::shutdown();
    }
    result
}

pub(crate) fn desktop_available() -> bool {
    use windows::Win32::{Foundation::HANDLE, System::StationsAndDesktops::*};
    if process::session(std::process::id()).ok() != Some(process::active_session()) {
        return false;
    }
    if crate::platform::capture::session_locked() != Some(false) {
        return false;
    }
    unsafe {
        let Ok(desktop) = OpenInputDesktop(DESKTOP_CONTROL_FLAGS(0), false, DESKTOP_READOBJECTS)
        else {
            return false;
        };
        let mut name = [0u16; 128];
        let result = GetUserObjectInformationW(
            HANDLE(desktop.0),
            UOI_NAME,
            Some(name.as_mut_ptr().cast()),
            256,
            None,
        );
        let _ = CloseDesktop(desktop);
        result.is_ok()
            && String::from_utf16_lossy(&name[..name.iter().position(|&c| c == 0).unwrap_or(128)])
                .eq_ignore_ascii_case("default")
    }
}
