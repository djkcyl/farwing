//! Authorized receiving-side file operations. Disk work lives in the user's executor.
pub(crate) mod agent;
mod filesystem;
mod metrics;
pub(crate) use metrics::Settings;
pub(crate) mod notices;
mod transfer;

use super::{BLOCK, Payload, WIRE, protocol::*, storage};
use anyhow::{Context, Result, ensure};
use prost::Message;
use std::{collections::HashMap, sync::Arc};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

pub(crate) use super::contract::Capabilities;
use super::contract::{self, Correlation, Direction, Kind, Message as Incoming};

#[derive(Clone, prost::Message)]
pub(crate) struct Packet {
    // A host-owned protobuf payload without outer seq/timestamp fields.
    // The network owner stamps every TEXT/FILE message with its shared clock.
    #[prost(bytes, tag = "1")]
    pub data: Vec<u8>,
    #[prost(bool, tag = "2")]
    pub file: bool,
}
// Reserve both maximal int64 envelope fields before queuing a response.
const PAYLOAD_LIMIT: usize = WIRE - 22;
impl Packet {
    pub(crate) fn stamped(&self, sequence: i64) -> Result<Vec<u8>> {
        let timestamp = i64::try_from(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)?
                .as_millis(),
        )?;
        let mut wire = Vec::with_capacity(self.data.len() + 22);
        prost::encoding::int64::encode(1, &sequence, &mut wire);
        prost::encoding::int64::encode(2, &timestamp, &mut wire);
        // Do not decode/re-encode large FileBlock payloads on the network task.
        wire.extend_from_slice(&self.data);
        ensure!(wire.len() < WIRE, "文件消息过大");
        Ok(wire)
    }
}
pub(crate) fn decode(bytes: &[u8]) -> Result<Option<Incoming>> {
    Incoming::decode(bytes)
}
pub(crate) fn settings_requested(bytes: &[u8]) -> Result<bool> {
    metrics::requested(bytes)
}
pub(super) fn failure(error: &anyhow::Error) -> i32 {
    if let Some(code) = error.downcast_ref::<ErrorCode>() {
        return code.0;
    }
    #[cfg(windows)]
    if let Some(e) = error.downcast_ref::<std::io::Error>() {
        return match e.raw_os_error() {
            Some(2 | 3) => 9,
            Some(5 | 32 | 33) => 4,
            Some(39 | 112) => 5,
            _ => 6,
        };
    }
    #[cfg(not(windows))]
    if let Some(e) = error.downcast_ref::<std::io::Error>() {
        use std::io::ErrorKind::*;
        return match e.kind() {
            NotFound | NotADirectory => 9,
            PermissionDenied | ResourceBusy | ReadOnlyFilesystem => 4,
            StorageFull | QuotaExceeded => 5,
            _ => 6,
        };
    }
    3
}
#[derive(Debug)]
pub(super) struct ErrorCode(pub i32);
impl std::fmt::Display for ErrorCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&super::error(self.0))
    }
}
impl std::error::Error for ErrorCode {}
fn response(header: Correlation, value: Res) -> Packet {
    Packet {
        file: false,
        data: contract::response(header, value).encode_to_vec(),
    }
}
fn request(header: Correlation, value: Req, file: bool) -> Packet {
    Packet {
        file,
        data: contract::request(header, value).encode_to_vec(),
    }
}
pub(crate) fn reject(bytes: &[u8], code: i32) -> Result<Option<Packet>> {
    if let Some(reply) = metrics::reply(bytes, false)? {
        return Ok(Some(reply));
    }
    let Some(v) = decode(bytes)? else {
        return Ok(None);
    };
    let Payload::Request(req) = v.payload else {
        return Ok(None);
    };
    let reply = match req {
        Req::ReceiveRequest(x) => Res::ReceiveResponse(FileTransferReceiveResponse {
            id: x.id,
            err: code,
            ..Default::default()
        }),
        Req::SendRequest(x) => Res::SendResponse(FileTransferSendResponse {
            id: x.id,
            err: code,
            ..Default::default()
        }),
        Req::ReadDir(x) => Res::FileDirectory(FileDirectory {
            id: x.id,
            path: x.path,
            file_error: code,
            ..Default::default()
        }),
        Req::FileAsk(x) => Res::FileConfirm(FileTransferConfirm {
            id: x.id,
            err: code,
            ..Default::default()
        }),
        Req::FileBlock(x) => Res::BlockConfirm(FileTransferBlockConfirm {
            id: x.id,
            block_id: x.block_id,
            err: code,
            ..Default::default()
        }),
        Req::RemoveDir(x) => Res::Result(FileTransferResult {
            id: x.id,
            file_error: code,
            err_msg: super::error(code),
        }),
        Req::Complete(x) => Res::Result(FileTransferResult {
            id: x.id,
            file_error: code,
            err_msg: super::error(code),
        }),
        Req::ClearSendTemp(_) | Req::DirSizeRead(_) => return Ok(None),
        _ => Res::OperationResult(FileOperationResult {
            file_error: code,
            err_msg: super::error(code),
            ..Default::default()
        }),
    };
    Ok(Some(response(v.header, reply)))
}
pub(crate) fn failure_reply(bytes: &[u8]) -> Result<bool> {
    let Some(v) = decode(bytes)? else {
        return Ok(false);
    };
    let code = match v.payload {
        Payload::Response(Res::ReceiveResponse(v)) => v.err,
        Payload::Response(Res::SendResponse(v)) => v.err,
        Payload::Response(Res::FileConfirm(v)) => v.err,
        Payload::Response(Res::BlockConfirm(v)) => v.err,
        Payload::Response(Res::OperationResult(v)) => v.file_error,
        Payload::Response(Res::Result(v)) => v.file_error,
        Payload::Response(Res::FileDirectory(v)) => v.file_error,
        _ => 1,
    };
    Ok((2..=12).contains(&code))
}
pub(crate) struct Engine {
    journal: notices::Journal,
    store: Arc<filesystem::Store>,
    output: mpsc::Sender<Packet>,
    tasks: HashMap<i32, Task>,
    work: tokio::task::JoinSet<Option<(i32, u64)>>,
    generation: u64,
    stop: CancellationToken,
    seen_requests: std::collections::HashSet<i64>,
    retired_tasks: std::collections::HashSet<i32>,
}
struct Task {
    generation: u64,
    header: Correlation,
    sender: mpsc::Sender<Incoming>,
    stop: CancellationToken,
    discard: Arc<std::sync::atomic::AtomicBool>,
    key: Option<String>,
}
impl Engine {
    pub fn new(
        scope: &str,
        output: mpsc::Sender<Packet>,
        journal: notices::Journal,
    ) -> Result<Self> {
        Ok(Self {
            journal,
            store: Arc::new(filesystem::Store::new(scope)?),
            output,
            tasks: HashMap::new(),
            work: tokio::task::JoinSet::new(),
            generation: 0,
            stop: CancellationToken::new(),
            seen_requests: Default::default(),
            retired_tasks: Default::default(),
        })
    }
    pub async fn receive(&mut self, bytes: &[u8], capabilities: Capabilities) -> Result<()> {
        while let Some(done) = self.work.try_join_next() {
            if let Ok(Some((id, generation))) = done
                && self
                    .tasks
                    .get(&id)
                    .is_some_and(|task| task.generation == generation)
            {
                self.tasks.remove(&id);
                self.retired_tasks.insert(id);
            }
        }
        // A task may drop its receiver just before JoinSet publishes completion.
        // Late traffic must not be delivered to that closed mailbox or retain
        // an admission slot after an execution task exits unexpectedly.
        let retired = &mut self.retired_tasks;
        self.tasks.retain(|id, task| {
            if task.sender.is_closed() {
                retired.insert(*id);
                false
            } else {
                true
            }
        });
        if metrics::requested(bytes)? {
            ensure!(self.work.len() < 32, "同时进行的文件操作过多");
            let bytes = bytes.to_vec();
            let output = self.output.clone();
            let stop = self.stop.child_token();
            self.work.spawn(async move {
                let reply = tokio::task::spawn_blocking(move || metrics::reply(&bytes, true)).await;
                match reply {
                    Ok(Ok(Some(packet))) => {
                        tokio::select! { _=stop.cancelled()=>{}, _=output.send(packet)=>{} }
                    }
                    error => tracing::warn!(?error, "host file settings unavailable"),
                }
                None
            });
            return Ok(());
        }
        let Some(message) = decode(bytes)? else {
            return Ok(());
        };
        match &message.payload {
            Payload::Response(Res::FileConfirm(v)) => {
                tracing::debug!(request=?message.header, id=?v.id, error=v.err, skip=v.skip, resume=v.resume_point, "host file confirmation ingress")
            }
            Payload::Response(Res::Result(v)) => {
                tracing::debug!(request=?message.header, id=?v.id, error=v.file_error, "host file result ingress")
            }
            Payload::Request(Req::Complete(v)) => {
                tracing::debug!(request=?message.header, id=?v.id, error=v.error, "host file completion ingress")
            }
            _ => {}
        }
        if let Payload::Request(Req::ClearSendTemp(clear)) = &message.payload {
            if let Some(task) = self
                .tasks
                .values()
                .find(|t| t.key.as_deref() == Some(clear.task_unique_id.as_str()))
            {
                task.discard
                    .store(true, std::sync::atomic::Ordering::Release);
                task.stop.cancel();
                return Ok(());
            }
        }
        let kind = message.payload.kind();
        let initial = matches!(kind, Kind::Start(_));
        if let Some(id) = message.payload.task().map(|x| x.task_id) {
            if let Some(task) = self.tasks.get_mut(&id) {
                ensure!(!initial, "文件任务编号重复");
                let request_header =
                    matches!(&message.payload, Payload::Request(_)).then_some(message.header);
                if let Err(error) = task.sender.try_send(message) {
                    task.stop.cancel();
                    return Err(anyhow::anyhow!("文件任务接收队列已满或关闭：{error}"));
                }
                if let Some(header) = request_header {
                    task.header = header;
                }
                return Ok(());
            }
        }
        let Payload::Request(ref req) = message.payload else {
            tracing::debug!(request=?message.header, task=?message.payload.task(),
                "host file response has no active owner");
            return Ok(());
        };
        if matches!(
            req,
            Req::FileAsk(_) | Req::FileBlock(_) | Req::Complete(_) | Req::DirSizeRead(_)
        ) {
            // TEXT pause/finish can retire a task before already-admitted FILE
            // packets arrive. They belong to that retired task, not a new one.
            if message
                .payload
                .task()
                .is_some_and(|id| self.retired_tasks.contains(&id.task_id))
            {
                return Ok(());
            }
            if let Some(reply) = reject(bytes, 3)? {
                self.output.try_send(reply)?;
            }
            return Ok(());
        }
        ensure!(self.work.len() < 32, "同时进行的文件操作过多");
        ensure!(
            self.seen_requests.len() < 65536 && self.retired_tasks.len() < 65536,
            "本次文件会话请求过多，请重新连接"
        );
        if let Some(id) = message.header.assigned() {
            ensure!(self.seen_requests.insert(id), "文件操作请求重复");
        }
        let store = self.store.clone();
        let output = self.output.clone();
        let stop = self.stop.child_token();
        let journal = self.journal.clone();
        if initial {
            ensure!(self.tasks.len() < 8, "同时传输的文件任务过多");
            let id = message.payload.task().context("文件任务缺少编号")?.task_id;
            // Only an explicit new initialization can reuse a retired peer ID.
            // Completion of the older worker must not remove this new owner.
            self.generation = self
                .generation
                .checked_add(1)
                .context("文件任务代次已用尽")?;
            let generation = self.generation;
            self.retired_tasks.remove(&id);
            let (tx, rx) = mpsc::channel(32);
            let discard = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let key = match &message.payload {
                Payload::Request(Req::ReceiveRequest(v)) => Some(v.task_unique_id.clone()),
                _ => None,
            };
            self.tasks.insert(
                id,
                Task {
                    generation,
                    header: message.header,
                    sender: tx,
                    stop: stop.clone(),
                    discard: discard.clone(),
                    key,
                },
            );
            self.work.spawn(async move {
                transfer::run(
                    message,
                    rx,
                    output,
                    store,
                    stop,
                    discard,
                    capabilities,
                    journal,
                )
                .await;
                Some((id, generation))
            });
        } else {
            self.work.spawn(async move {
                let header = message.header;
                let sending = stop.clone();
                let result = tokio::task::spawn_blocking(move || {
                    filesystem::operation(message, &store, &stop, capabilities)
                })
                .await;
                match result {
                    Ok(Ok(Some(packet))) => {
                        tokio::select! {_=sending.cancelled()=>{},_=output.send(packet)=>{}}
                    }
                    Ok(Ok(None)) => {}
                    error => {
                        tracing::warn!(?header, ?error, "host file management operation failed")
                    }
                }
                None
            });
        }
        Ok(())
    }
    pub async fn close(mut self) {
        self.stop.cancel();
        self.tasks.clear();
        while self.work.join_next().await.is_some() {}
    }
    fn revoked(&self) -> Vec<Packet> {
        self.tasks
            .iter()
            .map(|(&task, owner)| {
                response(
                    owner.header,
                    Res::Result(FileTransferResult {
                        id: Some(TaskId {
                            task_id: task,
                            file_index: -1,
                        }),
                        file_error: 4,
                        err_msg: super::error(4),
                    }),
                )
            })
            .collect()
    }
}
impl Drop for Engine {
    fn drop(&mut self) {
        self.stop.cancel();
    }
}
