//! 一个对端一条终端连接，多个本机终端窗口共用它。
//!
//! 被控端同时只接受一条会话，所以列表和所有 attach 必须走同一条
//! `kind = 8` 连接；服务把 `TERM` 帧按 session handle 分发给各个本机
//! 终端。服务还会在没有窗口、也没有界面轮询时自己退出，避免在设备列表
//! 上留下一个“有人正在控制”的会话。

use super::client::Connection;
use super::protocol::{Frame, FrameType, SessionInfo};
use crate::account::api::DeviceInfo;
use crate::account::client::AuthenticatedClient;
use anyhow::{Context, Result, bail, ensure};
use serde_json::json;
use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

const REQUEST_TIMEOUT: Duration = Duration::from_secs(45);
/// 界面关掉后，没有本机窗口的观察连接最多再留这么久。
const IDLE_TIMEOUT: Duration = Duration::from_secs(10);
const LIST_INTERVAL: Duration = Duration::from_secs(1);
/// 面板每 2 秒 touch 一次；超过这么久没 touch 就当面板关了，停止轮询列表。
const WATCH_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum Phase {
    #[default]
    Connecting,
    Ready,
    Failed,
}

#[derive(Clone, Default)]
pub(crate) struct Snapshot {
    pub(crate) phase: Phase,
    /// 至少完成过一次列表请求；空列表只有在 `listed` 后才有意义。
    pub(crate) listed: bool,
    pub(crate) sessions: Vec<SessionInfo>,
    pub(crate) error: Option<String>,
}

#[derive(Clone)]
pub(crate) struct AttachedInfo {
    pub(crate) handle: u32,
    pub(crate) name: String,
    pub(crate) shell: String,
}

#[derive(Debug)]
pub(crate) enum ClientInput {
    Data(Vec<u8>),
    Resize(u16, u16),
}

#[derive(Debug)]
pub(crate) enum ServiceOutput {
    Data(Vec<u8>),
    Error(String),
    Exit,
}

/// 本机窗口接到服务上的句柄。
pub(crate) struct Attach {
    pub(crate) ready: oneshot::Receiver<std::result::Result<AttachedInfo, String>>,
    pub(crate) input: mpsc::Sender<ClientInput>,
    pub(crate) output: mpsc::UnboundedReceiver<ServiceOutput>,
}

enum Command {
    List {
        reply: oneshot::Sender<std::result::Result<Vec<SessionInfo>, String>>,
    },
    Attach {
        session: Option<u32>,
        name: Option<String>,
        cols: u16,
        rows: u16,
        input: mpsc::Receiver<ClientInput>,
        output: mpsc::UnboundedSender<ServiceOutput>,
        reply: oneshot::Sender<std::result::Result<AttachedInfo, String>>,
    },
    Input {
        handle: u32,
        input: ClientInput,
    },
    ClientGone {
        handle: u32,
    },
    /// 设备详情页的终端面板还在看列表。
    Touch,
}

struct Service {
    target: String,
    client: Arc<AuthenticatedClient>,
    state: Arc<Mutex<Snapshot>>,
    commands: mpsc::UnboundedSender<Command>,
    stop: CancellationToken,
    done: CancellationToken,
}

#[derive(Clone)]
pub(crate) struct Handle {
    pub(crate) target: String,
    commands: mpsc::UnboundedSender<Command>,
}

impl Handle {
    pub(crate) async fn list(&self) -> Result<Vec<SessionInfo>> {
        let (reply, receiver) = oneshot::channel();
        self.commands
            .send(Command::List { reply })
            .map_err(|_| anyhow::anyhow!("终端服务已停止"))?;
        tokio::time::timeout(REQUEST_TIMEOUT, receiver)
            .await
            .context("等待终端会话列表超时")?
            .context("终端服务已停止")?
            .map_err(anyhow::Error::msg)
    }

    pub(crate) fn attach(
        &self,
        session: Option<u32>,
        name: Option<String>,
        cols: u16,
        rows: u16,
    ) -> Result<Attach> {
        let (input_tx, input_rx) = mpsc::channel(64);
        let (output_tx, output_rx) = mpsc::unbounded_channel();
        let (reply, ready) = oneshot::channel();
        self.commands
            .send(Command::Attach {
                session,
                name,
                cols,
                rows,
                input: input_rx,
                output: output_tx,
                reply,
            })
            .map_err(|_| anyhow::anyhow!("终端服务已停止"))?;
        Ok(Attach {
            ready,
            input: input_tx,
            output: output_rx,
        })
    }
}

fn lock<T>(value: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    value.lock().unwrap_or_else(|error| error.into_inner())
}

fn services() -> &'static Mutex<HashMap<String, Arc<Service>>> {
    static SERVICES: OnceLock<Mutex<HashMap<String, Arc<Service>>>> = OnceLock::new();
    SERVICES.get_or_init(Mutex::default)
}

fn key(controller: &str, target: &str) -> String {
    format!("{controller}:{target}")
}

pub(crate) fn snapshot(controller: &str, target: &str) -> Option<Snapshot> {
    lock(services())
        .get(&key(controller, target))
        .map(|service| lock(&service.state).clone())
}

pub(crate) fn start(client: Arc<AuthenticatedClient>, device: DeviceInfo) -> Handle {
    let key = key(&client.device_id(), &device.device_id);
    {
        let mut services = lock(services());
        services.retain(|_, service| !service.done.is_cancelled());
        if let Some(service) = services.get(&key) {
            if Arc::ptr_eq(&service.client, &client) {
                return Handle {
                    target: service.target.clone(),
                    commands: service.commands.clone(),
                };
            }
            let stale = services.remove(&key).expect("service was just found");
            stale.stop.cancel();
        }
        let state = Arc::new(Mutex::new(Snapshot {
            phase: Phase::Connecting,
            ..Default::default()
        }));
        let stop = client.ended().child_token();
        let done = CancellationToken::new();
        let task_done = done.clone();
        let (commands, receiver) = mpsc::unbounded_channel();
        let service = Arc::new(Service {
            target: device.device_id.clone(),
            client: Arc::clone(&client),
            state: Arc::clone(&state),
            commands: commands.clone(),
            stop: stop.clone(),
            done: done.clone(),
        });
        let shared_state = Arc::clone(&state);
        let command_sender = commands.clone();
        tokio::spawn(async move {
            let _done = task_done.drop_guard();
            let result = run(
                client,
                device,
                Arc::clone(&shared_state),
                stop.clone(),
                receiver,
                command_sender,
            )
            .await;
            if let Err(error) = result
                && !stop.is_cancelled()
            {
                tracing::warn!(error = %format!("{error:#}"), "remote terminal service stopped");
                let mut snapshot = lock(&shared_state);
                snapshot.phase = Phase::Failed;
                snapshot.error = Some(format!("{error:#}"));
            }
        });
        services.insert(key, service.clone());
        Handle {
            target: service.target.clone(),
            commands,
        }
    }
}

/// 由设备 ID 在账号里找到设备后启动。CLI 与 relay 都从这里进来。
pub(crate) async fn start_for(client: Arc<AuthenticatedClient>, device_id: &str) -> Result<Handle> {
    crate::account::api::validate_device_id(device_id)?;
    let list = client.list_devices().await?;
    let device = list
        .my_binded_devices
        .iter()
        .find(|device| device.device_id == device_id)
        .context("设备已不在当前账号中")?
        .clone();
    ensure!(
        matches!(device.platform, 1 | 4)
            && device.device_id != client.device_id()
            && device.is_connected()
            && device.controllable
            && device.controlled_support,
        "设备当前不可连接"
    );
    Ok(start(client, device))
}

pub(crate) fn touch(controller: &str, target: &str) {
    if let Some(service) = lock(services()).get(&key(controller, target)) {
        let _ = service.commands.send(Command::Touch);
    }
}

pub(crate) fn stop(controller: &str, target: &str) {
    if let Some(service) = lock(services()).remove(&key(controller, target)) {
        service.stop.cancel();
    }
}

pub(crate) async fn shutdown_all() {
    let services = std::mem::take(&mut *lock(services()));
    for service in services.values() {
        service.stop.cancel();
    }
    for service in services.values() {
        service.done.cancelled().await;
    }
}

struct Attachment {
    output: mpsc::UnboundedSender<ServiceOutput>,
    input: tokio::task::JoinHandle<()>,
}

type ListReply = oneshot::Sender<std::result::Result<Vec<SessionInfo>, String>>;

/// 列表请求不等回复：`ListSessionsResp` 和终端输出走同一条通道，在主循环里
/// 收，所以拉列表时按键照常发出去。同时最多一个请求在路上。
#[derive(Default)]
struct Lists {
    sent: Option<Instant>,
    waiting: Vec<ListReply>,
}

/// 主循环里按帧分发要用到的状态。
struct Router {
    attachments: HashMap<u32, Attachment>,
    lists: Lists,
    state: Arc<Mutex<Snapshot>>,
}

impl Router {
    async fn request_list(&mut self, connection: &mut Connection) -> Result<()> {
        if self.lists.sent.is_none() {
            connection.send(FrameType::ListSessionsReq, 0, &[]).await?;
            self.lists.sent = Some(Instant::now());
        }
        Ok(())
    }

    fn finish_list(&mut self, result: std::result::Result<Vec<SessionInfo>, String>) {
        self.lists.sent = None;
        if let Ok(sessions) = &result {
            let mut snapshot = lock(&self.state);
            snapshot.listed = true;
            snapshot.sessions = sessions.clone();
        }
        for reply in self.lists.waiting.drain(..) {
            let _ = reply.send(result.clone());
        }
    }

    /// 列表请求超时就把等着的人都打发掉，下一轮再发。
    fn expire_list(&mut self) {
        if self
            .lists
            .sent
            .is_some_and(|sent| sent.elapsed() >= REQUEST_TIMEOUT)
        {
            self.finish_list(Err("等待终端会话列表超时".to_owned()));
        }
    }
}

async fn run(
    client: Arc<AuthenticatedClient>,
    device: DeviceInfo,
    state: Arc<Mutex<Snapshot>>,
    stop: CancellationToken,
    mut commands: mpsc::UnboundedReceiver<Command>,
    command_sender: mpsc::UnboundedSender<Command>,
) -> Result<()> {
    let options = crate::media::preferences::load().unwrap_or_default();
    let mut connection = Connection::connect(&client, &device, options, &stop, None).await?;
    {
        let mut snapshot = lock(&state);
        snapshot.phase = Phase::Ready;
        snapshot.error = None;
    }
    let mut router = Router {
        attachments: HashMap::new(),
        lists: Lists::default(),
        state,
    };
    let mut last_used = Instant::now();
    let mut watched: Option<Instant> = None;
    let mut tick = tokio::time::interval(LIST_INTERVAL);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let result = loop {
        tokio::select! {
            biased;
            _ = stop.cancelled() => break Ok(()),
            command = commands.recv() => {
                let Some(command) = command else { break Ok(()); };
                last_used = Instant::now();
                if matches!(command, Command::Touch) {
                    let first = watched.is_none_or(|at| at.elapsed() >= WATCH_TIMEOUT);
                    watched = Some(Instant::now());
                    // 面板刚打开就拉一次，不用等下一个 tick。
                    if first && let Err(error) = router.request_list(&mut connection).await {
                        break Err(error);
                    }
                    continue;
                }
                if let Err(error) = handle_command(command, &mut connection, &mut router, &command_sender).await {
                    break Err(error);
                }
            }
            frame = connection.next_frame() => {
                let Some(frame) = frame else { break Err(anyhow::anyhow!("对端关闭了终端连接")); };
                router.route(frame);
            }
            _ = tick.tick() => {
                if router.attachments.is_empty() && last_used.elapsed() >= IDLE_TIMEOUT {
                    break Ok(());
                }
                router.expire_list();
                if watched.is_some_and(|at| at.elapsed() < WATCH_TIMEOUT)
                    && let Err(error) = router.request_list(&mut connection).await
                {
                    break Err(error);
                }
            }
        }
    };
    router.finish_list(Err("终端服务已停止".to_owned()));
    for (_, attachment) in router.attachments.drain() {
        attachment.input.abort();
        let _ = attachment.output.send(ServiceOutput::Exit);
    }
    let closed = connection.close().await;
    result.and(closed)
}

async fn handle_command(
    command: Command,
    connection: &mut Connection,
    router: &mut Router,
    command_sender: &mpsc::UnboundedSender<Command>,
) -> Result<()> {
    match command {
        Command::Touch => {}
        Command::List { reply } => {
            router.lists.waiting.push(reply);
            router.request_list(connection).await?;
        }
        Command::Attach {
            session,
            name,
            cols,
            rows,
            mut input,
            output,
            reply,
        } => {
            let result =
                request_open(connection, router, session, name.as_deref(), cols, rows).await;
            match result {
                Ok((handle, attached_name, shell)) => {
                    let command_sender = command_sender.clone();
                    let input = tokio::spawn(async move {
                        while let Some(input) = input.recv().await {
                            if command_sender
                                .send(Command::Input { handle, input })
                                .is_err()
                            {
                                break;
                            }
                        }
                        let _ = command_sender.send(Command::ClientGone { handle });
                    });
                    router.attachments.insert(
                        handle,
                        Attachment {
                            output: output.clone(),
                            input,
                        },
                    );
                    // OpenResp 的 replay 已经排在同一条通道后面，主循环下一轮会
                    // 把画面送给这个窗口。
                    let _ = reply.send(Ok(AttachedInfo {
                        handle,
                        name: attached_name,
                        shell,
                    }));
                }
                Err(error) => {
                    let _ = reply.send(Err(format!("{error:#}")));
                }
            }
        }
        Command::Input { handle, input } => {
            if !router.attachments.contains_key(&handle) {
                return Ok(());
            }
            match input {
                ClientInput::Data(data) => connection.send(FrameType::Stdin, handle, &data).await?,
                ClientInput::Resize(cols, rows) => {
                    let payload = serde_json::to_vec(&json!({"cols": cols, "rows": rows}))?;
                    connection.send(FrameType::Resize, handle, &payload).await?
                }
            }
        }
        Command::ClientGone { handle } => {
            if let Some(attachment) = router.attachments.remove(&handle) {
                attachment.input.abort();
                let _ = attachment.output.send(ServiceOutput::Exit);
                let _ = connection.send(FrameType::Detach, handle, &[]).await;
            }
        }
    }
    Ok(())
}

async fn request_open(
    connection: &mut Connection,
    router: &mut Router,
    session: Option<u32>,
    name: Option<&str>,
    cols: u16,
    rows: u16,
) -> Result<(u32, String, String)> {
    let requested = session.unwrap_or(0);
    let payload = serde_json::to_vec(&json!({
        "name": name,
        "cols": cols,
        "rows": rows,
    }))?;
    connection
        .send(FrameType::Open, requested, &payload)
        .await?;
    let deadline = tokio::time::Instant::now() + REQUEST_TIMEOUT;
    loop {
        let frame = tokio::time::timeout_at(deadline, connection.next_frame())
            .await
            .context("等待终端打开响应超时")?
            .context("对端关闭了终端连接")?;
        match frame.kind {
            FrameType::OpenResp => {
                let value: serde_json::Value =
                    serde_json::from_slice(&frame.payload).unwrap_or_default();
                let code = value.get("code").and_then(serde_json::Value::as_u64);
                if code.is_some_and(|code| code != 0) {
                    bail!(
                        "{}",
                        value
                            .get("message")
                            .and_then(serde_json::Value::as_str)
                            .unwrap_or("对端拒绝打开终端")
                    );
                }
                let handle = value
                    .get("session_id")
                    .and_then(serde_json::Value::as_u64)
                    .and_then(|value| u32::try_from(value).ok())
                    .unwrap_or(frame.session);
                ensure!(handle != 0, "对端没有返回终端编号");
                let name = value
                    .get("name")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or_default()
                    .to_owned();
                let shell = value
                    .get("shell")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or_default()
                    .to_owned();
                return Ok((handle, name, shell));
            }
            _ => router.route(frame),
        }
    }
}

impl Router {
    fn route(&mut self, frame: Frame) {
        let attachments = &mut self.attachments;
        match frame.kind {
            FrameType::ListSessionsResp => {
                let result = serde_json::from_slice(&frame.payload)
                    .map_err(|error| format!("终端会话列表不是 JSON：{error}"));
                self.finish_list(result);
            }
            FrameType::Stdout => {
                if let Some(attachment) = attachments.get(&frame.session) {
                    let _ = attachment.output.send(ServiceOutput::Data(frame.payload));
                }
            }
            FrameType::Exit => {
                if let Some(attachment) = attachments.remove(&frame.session) {
                    attachment.input.abort();
                    let _ = attachment.output.send(ServiceOutput::Exit);
                }
            }
            FrameType::Error => {
                let message = serde_json::from_slice::<serde_json::Value>(&frame.payload)
                    .ok()
                    .and_then(|value| {
                        value
                            .get("message")
                            .and_then(serde_json::Value::as_str)
                            .map(str::to_owned)
                    })
                    .unwrap_or_else(|| String::from_utf8_lossy(&frame.payload).into_owned());
                if let Some(attachment) = attachments.get(&frame.session) {
                    let _ = attachment.output.send(ServiceOutput::Error(message));
                }
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn router() -> Router {
        Router {
            attachments: HashMap::new(),
            lists: Lists::default(),
            state: Arc::default(),
        }
    }

    #[test]
    fn list_response_is_routed_without_blocking_output() {
        let mut router = router();
        let (output, mut received) = mpsc::unbounded_channel();
        router.attachments.insert(
            7,
            Attachment {
                output,
                input: tokio::runtime::Builder::new_current_thread()
                    .build()
                    .unwrap()
                    .spawn(async {}),
            },
        );
        let (reply, mut answer) = oneshot::channel();
        router.lists.waiting.push(reply);
        router.lists.sent = Some(Instant::now());
        // 列表回复之前到的输出照常交给窗口。
        router.route(Frame {
            kind: FrameType::Stdout,
            session: 7,
            payload: b"hi".to_vec(),
        });
        assert!(matches!(received.try_recv(), Ok(ServiceOutput::Data(data)) if data == b"hi"));
        router.route(Frame {
            kind: FrameType::ListSessionsResp,
            session: 0,
            payload: br#"[{"session_id":7,"state":"running","name":"a"}]"#.to_vec(),
        });
        let sessions = answer.try_recv().unwrap().unwrap();
        assert_eq!(sessions[0].session_id, 7);
        assert!(router.lists.sent.is_none());
        let snapshot = lock(&router.state).clone();
        assert!(snapshot.listed);
        assert_eq!(snapshot.sessions.len(), 1);
    }

    #[test]
    fn malformed_list_keeps_previous_snapshot() {
        let mut router = router();
        lock(&router.state).sessions = vec![SessionInfo::default()];
        let (reply, mut answer) = oneshot::channel();
        router.lists.waiting.push(reply);
        router.route(Frame {
            kind: FrameType::ListSessionsResp,
            session: 0,
            payload: b"nope".to_vec(),
        });
        assert!(answer.try_recv().unwrap().is_err());
        assert_eq!(lock(&router.state).sessions.len(), 1);
        assert!(!lock(&router.state).listed);
    }
}
