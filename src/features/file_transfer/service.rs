use super::*;
use crate::account::client::AuthenticatedClient;
use crate::session::controller::ControllerConnection;
use serde::{Deserialize, Serialize};
use std::{collections::HashSet, path::PathBuf};
use storage::Store;

#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum Direction {
    Upload,
    Download,
}
#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum TaskState {
    Queued,
    Running,
    Paused,
    Failed,
    Done,
    Cancelled,
    Skipped,
}
impl TaskState {
    pub fn label(self) -> &'static str {
        match self {
            Self::Queued => "等待传输",
            Self::Running => "传输中",
            Self::Paused => "已暂停",
            Self::Failed => "失败",
            Self::Done => "已完成",
            Self::Cancelled => "已取消",
            Self::Skipped => "已跳过",
        }
    }
}
#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct PartialFile {
    pub info: FileInfo,
    pub target: String,
    pub done: bool,
    pub skipped: bool,
}
#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct Record {
    pub key: String,
    pub direction: Direction,
    pub source: String,
    pub destination: String,
    pub policy: i32,
    pub state: TaskState,
    pub error: Option<String>,
    pub total: u64,
    #[serde(default)]
    pub confirmed: u64,
    pub folder: String,
    pub files: Vec<FileInfo>,
    pub partial: Vec<PartialFile>,
    pub local_root: Option<PathBuf>,
    pub initialized: bool,
    pub started: bool,
}
impl Record {
    pub fn new(direction: Direction, source: String, destination: String, policy: i32) -> Self {
        Self {
            key: uuid::Uuid::new_v4().to_string(),
            direction,
            source,
            destination,
            policy,
            state: TaskState::Queued,
            error: None,
            total: 0,
            confirmed: 0,
            folder: String::new(),
            files: vec![],
            partial: vec![],
            local_root: None,
            initialized: false,
            started: false,
        }
    }
    pub(super) fn completed_bytes(&self) -> u64 {
        if self.state == TaskState::Done {
            self.total
        } else {
            self.partial
                .iter()
                .filter(|p| p.done || p.skipped)
                .map(|p| p.info.size)
                .sum()
        }
    }
}
#[derive(Clone, Default)]
pub(crate) struct Progress {
    pub bytes: u64,
    pub speed: f64,
    sample: Option<(std::time::Instant, u64)>,
}
#[derive(Clone, Default)]
pub(crate) struct Listing {
    pub path: String,
    pub entries: Arc<Vec<FileEntry>>,
    pub busy: bool,
    pub error: Option<String>,
    pub revision: u64,
}
#[derive(Clone, Default)]
pub(crate) struct Snapshot {
    pub mutation: u64,
    pub connected: bool,
    pub connection_generation: u64,
    pub connecting: bool,
    pub error: Option<String>,
    pub local: Listing,
    pub remote: Listing,
    pub records: Vec<Arc<Record>>,
    pub progress: HashMap<String, Progress>,
    pub takeover: Option<crate::account::api::DeviceInfo>,
    pub operation_busy: bool,
}
pub(crate) enum Command {
    Connect,
    Takeover(crate::session::controller::takeover::Approval),
    Browse(bool, String),
    Add(Vec<Record>),
    Pause(String),
    Resume(String),
    Cancel(String),
    Remove(String),
    PauseAll,
    ResumeAll,
    ClearCompleted,
    Create(bool, String, String),
    Rename(bool, String, String),
    Delete(bool, String, bool),
}
struct Service {
    state: Arc<Mutex<Snapshot>>,
    commands: mpsc::Sender<Command>,
    stop: CancellationToken,
    windows: tokio::sync::watch::Sender<usize>,
}

// Clones share one attachment; reopening creates a new attachment. An old
// window finishing destruction cannot detach a newly opened window.
#[derive(Clone)]
pub(crate) struct Handle {
    service: Arc<Service>,
    _window: Arc<WindowAttachment>,
}
struct WindowAttachment(tokio::sync::watch::Sender<usize>);
impl Drop for WindowAttachment {
    fn drop(&mut self) {
        self.0.send_modify(|count| *count -= 1);
    }
}
impl Handle {
    fn attach(service: Arc<Service>) -> Self {
        service.windows.send_modify(|count| *count += 1);
        Self {
            _window: Arc::new(WindowAttachment(service.windows.clone())),
            service,
        }
    }
    pub fn snapshot(&self) -> Snapshot {
        lock(&self.service.state).clone()
    }
    pub fn send(&self, c: Command) -> Result<()> {
        self.service
            .commands
            .try_send(c)
            .map_err(|_| anyhow::anyhow!("操作队列已满，请稍后再试"))
    }
}
pub(super) struct Repository {
    store: Store,
    records: Mutex<Vec<Record>>,
    view: Arc<Mutex<Snapshot>>,
}
impl Repository {
    pub fn save(&self, record: &Record) -> Result<()> {
        let mut records = lock(&self.records);
        let mut next = records.clone();
        if let Some(v) = next.iter_mut().find(|v| v.key == record.key) {
            *v = record.clone()
        } else {
            next.push(record.clone())
        }
        self.store.save(&next)?;
        *records = next;
        let mut view = lock(&self.view);
        let completed = record.state == TaskState::Done
            && !view
                .records
                .iter()
                .any(|r| r.key == record.key && r.state == TaskState::Done);
        if let Some(item) = view.records.iter_mut().find(|r| r.key == record.key) {
            *item = Arc::new(record.clone());
        } else {
            view.records.push(Arc::new(record.clone()));
        }
        if completed {
            view.mutation = view.mutation.wrapping_add(1);
        }
        Ok(())
    }
    pub fn report(&self, error: String) {
        lock(&self.view).error = Some(error);
    }
    pub fn transferred(&self, key: &str) -> u64 {
        lock(&self.view).progress.get(key).map_or(0, |p| p.bytes)
    }
    pub fn progress(&self, key: &str, bytes: u64, finished: bool) {
        let mut s = lock(&self.view);
        let p = s.progress.entry(key.into()).or_default();
        let now = std::time::Instant::now();
        if let Some((old, n)) = p.sample {
            let dt = now.duration_since(old).as_secs_f64();
            if dt >= 0.5 {
                p.speed = bytes.saturating_sub(n) as f64 / dt;
                p.sample = Some((now, bytes));
            }
        } else {
            p.sample = Some((now, bytes));
        }
        p.bytes = bytes;
        if finished {
            p.speed = 0.;
            p.sample = None;
        }
    }
    fn get(&self, key: &str) -> Option<Record> {
        lock(&self.records).iter().find(|r| r.key == key).cloned()
    }
}
fn jobs() -> &'static Mutex<Vec<(CancellationToken, CancellationToken)>> {
    static V: std::sync::OnceLock<Mutex<Vec<(CancellationToken, CancellationToken)>>> =
        std::sync::OnceLock::new();
    V.get_or_init(Mutex::default)
}
fn service_gate(key: String) -> Arc<tokio::sync::Mutex<()>> {
    static GATES: std::sync::OnceLock<Mutex<HashMap<String, Weak<tokio::sync::Mutex<()>>>>> =
        std::sync::OnceLock::new();
    let mut gates = lock(GATES.get_or_init(Mutex::default));
    gates.retain(|_, v| v.strong_count() > 0);
    if let Some(gate) = gates.get(&key).and_then(Weak::upgrade) {
        return gate;
    }
    let gate = Arc::new(tokio::sync::Mutex::new(()));
    gates.insert(key, Arc::downgrade(&gate));
    gate
}
fn services() -> &'static Mutex<HashMap<String, Arc<Service>>> {
    static SERVICES: std::sync::OnceLock<Mutex<HashMap<String, Arc<Service>>>> =
        std::sync::OnceLock::new();
    SERVICES.get_or_init(Mutex::default)
}
pub(crate) fn is_transferring(controller: &str, target: &str) -> bool {
    let service = lock(services())
        .get(&format!("{controller}:{target}"))
        .cloned();
    let Some(service) = service else {
        return false;
    };
    if service.stop.is_cancelled() || service.commands.is_closed() {
        return false;
    }
    lock(&service.state)
        .records
        .iter()
        .any(|record| matches!(record.state, TaskState::Queued | TaskState::Running))
}

pub(crate) async fn shutdown_all() {
    let tasks = std::mem::take(&mut *lock(jobs()));
    for (stop, _) in &tasks {
        stop.cancel();
    }
    lock(services()).clear();
    for (_, done) in tasks {
        done.cancelled().await;
    }
}
pub(crate) fn start(
    client: Arc<AuthenticatedClient>,
    device: crate::account::api::DeviceInfo,
    options: crate::media::ConnectionMediaOptions,
) -> Handle {
    let mut services = lock(services());
    services.retain(|_, s| !s.stop.is_cancelled() && !s.commands.is_closed());
    let key = format!("{}:{}", client.device_id(), device.device_id);
    if let Some(service) = services.get(&key) {
        return Handle::attach(service.clone());
    }
    let state = Arc::new(Mutex::new(Snapshot::default()));
    let (stop, done) = (client.ended().child_token(), CancellationToken::new());
    let (commands, rx) = mpsc::channel(64);
    let (windows, window_rx) = tokio::sync::watch::channel(0);
    let service = Arc::new(Service {
        state: state.clone(),
        commands,
        stop: stop.clone(),
        windows,
    });
    let handle = Handle::attach(service.clone());
    services.insert(key.clone(), service.clone());
    lock(jobs()).retain(|(_, d)| !d.is_cancelled());
    lock(jobs()).push((stop.clone(), done.clone()));
    tokio::spawn(async move {
        let _done = done.drop_guard();
        if let Err(e) = run(
            client,
            device,
            options,
            state.clone(),
            rx,
            window_rx,
            stop.clone(),
        )
        .await
        {
            lock(&state).error = Some(format!("{e:#}"));
        }
        {
            let mut s = lock(&state);
            s.connected = false;
            s.connecting = false;
            s.operation_busy = false;
        }
        stop.cancel();
        let mut entries = lock(self::services());
        if entries
            .get(&key)
            .is_some_and(|entry| Arc::ptr_eq(entry, &service))
        {
            entries.remove(&key);
        }
    });
    handle
}

async fn connect(
    client: &AuthenticatedClient,
    device: &crate::account::api::DeviceInfo,
    options: crate::media::ConnectionMediaOptions,
    stop: &CancellationToken,
    approval: Option<crate::session::controller::takeover::Approval>,
) -> Result<ControllerConnection> {
    let list = tokio::select! {biased;_=stop.cancelled()=>anyhow::bail!("已取消"),v=client.list_devices()=>v?};
    let d = list
        .my_binded_devices
        .iter()
        .find(|d| d.device_id == device.device_id)
        .context("设备已不在当前账号中")?;
    ensure!(
        matches!(d.platform, 1 | 4)
            && d.device_id != client.device_id()
            && d.is_connected()
            && d.controllable
            && d.controlled_support,
        "设备当前不允许文件连接"
    );
    let policy = client.feature_catalog().policy(d.platform, &d.version_name);
    ControllerConnection::connect_files(client, d, policy, options, stop, approval).await
}
async fn run(
    client: Arc<AuthenticatedClient>,
    device: crate::account::api::DeviceInfo,
    options: crate::media::ConnectionMediaOptions,
    state: Arc<Mutex<Snapshot>>,
    mut commands: mpsc::Receiver<Command>,
    mut windows: tokio::sync::watch::Receiver<usize>,
    stop: CancellationToken,
) -> Result<()> {
    let gate = service_gate(format!("{}:{}", client.device_id(), device.device_id));
    let _owner = tokio::select! {biased;_=stop.cancelled()=>return Ok(()),g=gate.lock()=>g};
    let store = client.file_transfer_store(&device.device_id)?;
    let mut records = store.load()?;
    for r in &mut records {
        if matches!(r.state, TaskState::Running | TaskState::Queued) {
            r.state = TaskState::Paused;
        }
    }
    lock(&state).records = records.iter().cloned().map(Arc::new).collect();
    let repo = Arc::new(Repository {
        store,
        records: Mutex::new(records),
        view: state.clone(),
    });
    let mut wire: Option<Arc<Transport>> = None;
    let mut alive: Option<tokio::task::JoinHandle<Result<()>>> = None;
    let mut end: Option<tokio::sync::oneshot::Sender<()>> = None;
    let mut running = HashMap::<String, (CancellationToken, tokio::task::JoinHandle<()>)>::new();
    let mut connecting = false;
    let mut had_window = false;
    let mut approval = None;
    let mut tick = tokio::time::interval(Duration::from_millis(150));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut queries = tokio::task::JoinSet::new();
    let mut remote_query = stop.child_token();
    let mut local_query = stop.child_token();
    loop {
        let visible = *windows.borrow_and_update() > 0;
        if visible && !had_window && wire.is_none() {
            connecting = true;
        }
        if !visible && had_window {
            remote_query.cancel();
            local_query.cancel();
            let mut s = lock(&state);
            s.local.busy = false;
            s.remote.busy = false;
            s.takeover = None;
        }
        had_window = visible;
        if !visible
            && running.is_empty()
            && !lock(&repo.records)
                .iter()
                .any(|r| r.state == TaskState::Queued)
        {
            connecting = false;
            approval = None;
        }
        if connecting && !stop.is_cancelled() {
            {
                let mut s = lock(&state);
                s.connecting = true;
                s.error = None;
                s.takeover = None;
            }
            let attempt_stop = stop.child_token();
            let attempt_future = async {
                let c = connect(&client, &device, options, &attempt_stop, approval.take()).await?;
                let ready = tokio::select! {biased;_=attempt_stop.cancelled()=>Err(anyhow::anyhow!("已取消")),v=c.wait_port_mapping_ready()=>v};
                if let Err(e) = ready {
                    let _ = c.close().await;
                    return Err(e);
                }
                if !c.stream_control_handle().file_transfer().supported() {
                    let _ = c.close().await;
                    anyhow::bail!("被控端不支持当前文件传输协议");
                }
                Ok(c)
            };
            tokio::pin!(attempt_future);
            let attempt = loop {
                tokio::select! {
                    result=&mut attempt_future=>break result,
                    changed=windows.changed()=>{
                        if changed.is_err() || *windows.borrow() == 0 {attempt_stop.cancel();}
                    }
                }
            };
            let cancelled = attempt_stop.is_cancelled();
            match attempt {
                Ok(c) => {
                    let w = c.stream_control_handle().file_transfer().clone();
                    let (tx, rx) = tokio::sync::oneshot::channel();
                    end = Some(tx);
                    alive = Some(tokio::spawn(c.keep_alive(rx)));
                    wire = Some(w);
                    let mut s = lock(&state);
                    s.connected = true;
                    s.connection_generation = s.connection_generation.wrapping_add(1);
                    s.error = None;
                }
                Err(e) if !cancelled => {
                    let mut s = lock(&state);
                    s.error = Some(format!("{e:#}"));
                    if let Some(v) =
                        e.downcast_ref::<crate::session::controller::takeover::Required>()
                    {
                        s.takeover = Some(v.0.clone());
                    }
                }
                Err(_) => {}
            }
            lock(&state).connecting = false;
            connecting = cancelled && *windows.borrow() > 0 && !stop.is_cancelled();
        }
        if wire.as_ref().is_some_and(|w| !w.allowed()) {
            for (c, _) in running.values() {
                c.cancel();
            }
            remote_query.cancel();
            if let Err(error) = pause_queued(&repo) {
                repo.report(error.to_string());
            }
            if let Some(tx) = end.take() {
                let _ = tx.send(());
            }
            if let Some(task) = alive.take() {
                let _ = task.await;
            }
            wire = None;
            let mut s = lock(&state);
            s.connected = false;
            s.error = Some("被控端已关闭连接权限，传输已暂停".into());
        }
        if alive.as_ref().is_some_and(|a| a.is_finished()) {
            let result = alive.take().unwrap().await;
            wire = None;
            end = None;
            for (c, _) in running.values() {
                c.cancel();
            }
            if let Err(error) = pause_queued(&repo) {
                repo.report(error.to_string());
            }
            remote_query.cancel();
            let mut s = lock(&state);
            s.connected = false;
            s.remote.busy = false;
            s.error = Some(match result {
                Ok(Err(e)) => format!("连接已结束：{e}"),
                _ => "连接已结束，任务已暂停".into(),
            });
        }
        let finished = running
            .iter()
            .filter(|(_, (_, t))| t.is_finished())
            .map(|(k, _)| k.clone())
            .collect::<Vec<_>>();
        for key in finished {
            if let Some((_, t)) = running.remove(&key) {
                if let Err(e) = t.await {
                    let message = format!("文件工作线程已停止：{e}");
                    if let Some(mut record) = repo.get(&key) {
                        record.state = TaskState::Failed;
                        record.error = Some(message.clone());
                        if let Err(error) = repo.save(&record) {
                            repo.report(error.to_string());
                        }
                    }
                    repo.report(message);
                }
            }
        }
        if stop.is_cancelled() {
            break;
        }
        if let Some(w) = &wire {
            // Files sharing a destination must choose duplicate names serially.
            // Separate destinations and uploads can still run concurrently.
            let mut destinations = lock(&repo.records)
                .iter()
                .filter(|r| r.direction == Direction::Download && running.contains_key(&r.key))
                .map(|r| r.destination.to_lowercase())
                .collect::<HashSet<_>>();
            let pending = lock(&repo.records)
                .iter()
                .filter(|r| r.state == TaskState::Queued && !running.contains_key(&r.key))
                .filter(|r| {
                    r.direction != Direction::Download
                        || destinations.insert(r.destination.to_lowercase())
                })
                .take(4usize.saturating_sub(running.len()))
                .cloned()
                .collect::<Vec<_>>();
            for r in pending {
                let token = stop.child_token();
                let t = tokio::spawn(tasks::run(
                    w.clone(),
                    r.clone(),
                    repo.clone(),
                    token.clone(),
                ));
                running.insert(r.key, (token, t));
            }
        }
        let attached = *windows.borrow() > 0;
        let queued = lock(&repo.records)
            .iter()
            .any(|r| r.state == TaskState::Queued);
        if !attached && running.is_empty() && !queued && queries.is_empty() && commands.is_empty() {
            // Release only the file-transfer connection lease; viewing and port
            // forwarding keep their own leases on the shared device session.
            if let Some(tx) = end.take() {
                let _ = tx.send(());
            }
            if let Some(task) = alive.take() {
                let _ = task.await;
            }
            wire = None;
            let mut s = lock(&state);
            s.connected = false;
            s.connecting = false;
            s.takeover = None;
        }
        let ticking =
            attached || wire.is_some() || !running.is_empty() || !queries.is_empty() || connecting;
        let command = tokio::select! {
            biased;
            _=stop.cancelled()=>continue,
            _=windows.changed()=>continue,
            _=tick.tick(),if ticking=>continue,
            Some(result)=queries.join_next(),if !queries.is_empty()=>{if let Err(e)=result{repo.report(e.to_string());}continue;},
            v=commands.recv()=>match v{Some(c)=>c,None=>break}
        };
        if *windows.borrow() == 0
            && matches!(
                command,
                Command::Browse(..) | Command::Connect | Command::Takeover(..)
            )
        {
            continue;
        }

        let result: Result<()> = async {
            match command {
                Command::PauseAll => {
                    pause_all(&repo, &mut running).await?;
                }
                Command::ResumeAll => {
                    ensure!(wire.is_some(), "请先连接设备");
                    let mut records = lock(&repo.records);
                    let mut next = records.clone();
                    for r in &mut next {
                        if matches!(r.state, TaskState::Paused | TaskState::Failed)
                            && !running.contains_key(&r.key)
                        {
                            r.state = TaskState::Queued;
                            r.error = None;
                        }
                    }
                    repo.store.save(&next)?;
                    *records = next;
                    lock(&state).records = records.iter().cloned().map(Arc::new).collect();
                }
                Command::ClearCompleted => {
                    let mut records = lock(&repo.records);
                    let next = records
                        .iter()
                        .filter(|r| {
                            !matches!(
                                r.state,
                                TaskState::Done | TaskState::Cancelled | TaskState::Skipped
                            )
                        })
                        .cloned()
                        .collect::<Vec<_>>();
                    repo.store.save(&next)?;
                    *records = next;
                    let mut s = lock(&state);
                    s.records = records.iter().cloned().map(Arc::new).collect();
                    s.progress
                        .retain(|key, _| records.iter().any(|r| r.key == *key));
                }
                Command::Connect => {
                    if wire.is_none() {
                        connecting = true;
                    }
                }
                Command::Takeover(v) => {
                    if wire.is_none() {
                        approval = Some(v);
                        connecting = true;
                    }
                }
                Command::Browse(remote, path) => {
                    ensure!(!path.contains('\0') && path.len() < 65536, "目录路径无效");
                    let token = if remote {
                        remote_query.cancel();
                        remote_query = stop.child_token();
                        remote_query.clone()
                    } else {
                        local_query.cancel();
                        local_query = stop.child_token();
                        local_query.clone()
                    };
                    let rev = {
                        let mut s = lock(&state);
                        let l = if remote { &mut s.remote } else { &mut s.local };
                        l.revision += 1;
                        l.path = path.clone();
                        l.entries = Arc::default();
                        l.busy = true;
                        l.error = None;
                        l.revision
                    };
                    let state = state.clone();
                    let w = wire.clone();
                    queries.spawn(async move {
                        let result = if remote {
                            match w {
                                Some(w) => tasks::directory(w, path, token.clone()).await,
                                None => Err(anyhow::anyhow!("文件连接未就绪")),
                            }
                        } else {
                            tokio::task::spawn_blocking(move || local_list(&path))
                                .await
                                .unwrap_or_else(|e| Err(e.into()))
                        };
                        if token.is_cancelled() {
                            return;
                        }
                        let mut s = lock(&state);
                        let l = if remote { &mut s.remote } else { &mut s.local };
                        if l.revision != rev {
                            return;
                        }
                        l.busy = false;
                        match result {
                            Ok(v) => {
                                l.entries = Arc::new(v);
                                l.error = None
                            }
                            Err(e) => {
                                l.entries = Arc::default();
                                l.error = Some(e.to_string())
                            }
                        }
                    });
                }
                Command::Add(records) => {
                    ensure!(wire.is_some(), "请先连接设备");
                    ensure!(
                        lock(&repo.records).len() + records.len() <= 256,
                        "传输记录已满"
                    );
                    for r in records {
                        ensure!(
                            matches!(r.policy, 1..=3)
                                && !r.source.is_empty()
                                && !r.destination.is_empty(),
                            "传输参数无效"
                        );
                        ensure!(
                            !lock(&repo.records)
                                .iter()
                                .any(|old| old.direction == r.direction
                                    && old.source == r.source
                                    && old.destination == r.destination
                                    && matches!(
                                        old.state,
                                        TaskState::Queued | TaskState::Running | TaskState::Paused
                                    )),
                            "同一传输任务已经存在，请继续或取消原任务"
                        );
                        repo.save(&r)?;
                    }
                }
                Command::Pause(key) => {
                    if let Some((c, t)) = running.remove(&key) {
                        c.cancel();
                        t.await?;
                    } else if let Some(mut r) = repo.get(&key) {
                        if r.state == TaskState::Queued {
                            r.state = TaskState::Paused;
                            repo.save(&r)?;
                        }
                    }
                }
                Command::Resume(key) => {
                    ensure!(wire.is_some(), "请先重新连接设备");
                    ensure!(!running.contains_key(&key), "任务正在停止，请稍后继续");
                    let mut r = repo.get(&key).context("任务不存在")?;
                    ensure!(
                        matches!(r.state, TaskState::Paused | TaskState::Failed),
                        "任务不可继续"
                    );
                    r.state = TaskState::Queued;
                    r.error = None;
                    repo.save(&r)?;
                }
                Command::Cancel(key) | Command::Remove(key) => {
                    if let Some((c, t)) = running.remove(&key) {
                        c.cancel();
                        t.await?;
                    }
                    let mut r = repo.get(&key).context("任务不存在")?;
                    if !matches!(
                        r.state,
                        TaskState::Done | TaskState::Cancelled | TaskState::Skipped
                    ) {
                        let cleanup: Result<()> = async {
                            if r.direction == Direction::Download {
                                if let Some(root) = &r.local_root {
                                    storage::cleanup(root, &r.key, &r.partial)?;
                                }
                            } else if r.started {
                                let w = wire
                                    .as_ref()
                                    .context("请连接设备后取消，以清理远端临时文件")?;
                                tasks::operation(
                                    w.clone(),
                                    Req::ClearSendTemp(ClearSendTemp {
                                        task_unique_id: r.key.clone(),
                                    }),
                                    stop.child_token(),
                                )
                                .await?;
                            }
                            Ok(())
                        }
                        .await;
                        r.state = TaskState::Cancelled;
                        r.error = cleanup
                            .err()
                            .map(|e| format!("已取消；临时文件清理未完成：{e:#}"));
                        repo.save(&r)?;
                    } else {
                        let mut records = lock(&repo.records);
                        let next = records
                            .iter()
                            .filter(|v| v.key != key)
                            .cloned()
                            .collect::<Vec<_>>();
                        repo.store.save(&next)?;
                        *records = next;
                        lock(&state).records.retain(|r| r.key != key);
                    }
                }
                c @ (Command::Create(..) | Command::Rename(..) | Command::Delete(..)) => {
                    ensure!(!lock(&state).operation_busy, "请等待文件操作完成");
                    lock(&state).operation_busy = true;
                    let repo = repo.clone();
                    let w = wire.clone();
                    let token = stop.child_token();
                    queries.spawn(async move {
                        let result = manage(c, w, token).await;
                        let mut s = lock(&repo.view);
                        s.operation_busy = false;
                        s.mutation = s.mutation.wrapping_add(1);
                        if let Err(e) = result {
                            s.error = Some(format!("{e:#}"));
                        } else {
                            s.error = None;
                            s.remote.revision += 1;
                            s.local.revision += 1;
                        }
                    });
                }
            }
            Ok(())
        }
        .await;
        if let Err(e) = result {
            repo.report(format!("{e:#}"));
        }
    }
    remote_query.cancel();
    local_query.cancel();
    if let Err(error) = pause_queued(&repo) {
        repo.report(error.to_string());
    }
    for (c, _) in running.values() {
        c.cancel();
    }
    // Release this lease before joining workers. The final owner terminates
    // SCTP and wakes admissions; other viewing/forwarding owners stay alive.
    if let Some(tx) = end {
        let _ = tx.send(());
    }
    for (_, (_, t)) in running {
        let _ = t.await;
    }
    while queries.join_next().await.is_some() {}
    if let Some(t) = alive {
        let _ = t.await;
    }
    Ok(())
}
async fn pause_all(
    repo: &Repository,
    running: &mut HashMap<String, (CancellationToken, tokio::task::JoinHandle<()>)>,
) -> Result<()> {
    for (c, _) in running.values() {
        c.cancel();
    }
    let mut failure = None;
    for (key, (_, task)) in running.drain() {
        if let Err(error) = task.await {
            let message = format!("文件工作线程已停止：{error}");
            if let Some(mut record) = repo.get(&key) {
                record.state = TaskState::Failed;
                record.error = Some(message.clone());
                if let Err(error) = repo.save(&record) {
                    failure.get_or_insert_with(|| error.to_string());
                }
            }
            failure.get_or_insert(message);
        }
    }
    let mut records = lock(&repo.records);
    let mut next = records.clone();
    for r in &mut next {
        if r.state == TaskState::Queued {
            r.state = TaskState::Paused;
        }
    }
    repo.store.save(&next)?;
    *records = next;
    lock(&repo.view).records = records.iter().cloned().map(Arc::new).collect();
    // An error from one worker must not detach the remainder,
    // otherwise ResumeAll can overlap their unfinished cleanup.
    if let Some(error) = failure {
        anyhow::bail!(error);
    }
    Ok(())
}

fn pause_queued(repo: &Repository) -> Result<()> {
    let queued = lock(&repo.records)
        .iter()
        .filter(|r| r.state == TaskState::Queued)
        .cloned()
        .collect::<Vec<_>>();
    for mut record in queued {
        record.state = TaskState::Paused;
        repo.save(&record)?;
    }
    Ok(())
}

pub(super) fn local_list(path: &str) -> Result<Vec<FileEntry>> {
    if path == ":/" {
        #[cfg(windows)]
        let mut entries: Vec<_> = ('A'..='Z')
            .filter_map(|c| {
                let path = format!("{c}:\\");
                PathBuf::from(&path).is_dir().then(|| FileEntry {
                    entry_type: 3,
                    name: format!("({c}:)"),
                    full_path: path,
                    icon_type: "disk".into(),
                    ..Default::default()
                })
            })
            .collect();
        // Unix has one tree; its root takes the place of the drive list.
        #[cfg(not(windows))]
        let mut entries = vec![FileEntry {
            entry_type: 3,
            name: "文件系统 (/)".into(),
            full_path: "/".into(),
            icon_type: "disk".into(),
            ..Default::default()
        }];
        entries.extend(
            crate::platform::file_locations::known_folders()
                .into_iter()
                .map(|(path, name, icon)| FileEntry {
                    entry_type: 0,
                    name: name.into(),
                    full_path: path.to_string_lossy().into_owned(),
                    icon_type: icon.into(),
                    ..Default::default()
                }),
        );
        return Ok(entries);
    }
    let root = storage::canonical_dir(std::path::Path::new(path))?;
    let mut entries = vec![];
    for e in std::fs::read_dir(root)? {
        let e = e?;
        let m = std::fs::symlink_metadata(e.path())?;
        let link = storage::is_link(&m);
        entries.push(FileEntry {
            entry_type: if m.is_dir() {
                if link { 2 } else { 0 }
            } else if link {
                5
            } else {
                4
            },
            name: e.file_name().to_string_lossy().into_owned(),
            size: m.len(),
            modified_time: storage::modified(&m),
            full_path: e.path().to_string_lossy().into_owned(),
            // Official FileEntry carries the extension including its leading dot.
            // Directories have no extension marker; special root locations above
            // use the protocol's named icons instead.
            icon_type: if m.is_dir() {
                String::new()
            } else {
                e.path()
                    .extension()
                    .filter(|v| !v.is_empty())
                    .map(|v| format!(".{}", v.to_string_lossy()))
                    .unwrap_or_default()
            },
        });
        ensure!(entries.len() <= storage::MAX_FILES, "目录项目过多");
    }
    entries.sort_by(|a, b| {
        (a.entry_type >= 4, a.name.to_lowercase()).cmp(&(b.entry_type >= 4, b.name.to_lowercase()))
    });
    Ok(entries)
}
async fn manage(c: Command, wire: Option<Arc<Transport>>, stop: CancellationToken) -> Result<()> {
    let (remote, path, name, kind) = match c {
        Command::Create(r, p, n) => (r, p, n, 0),
        Command::Rename(r, p, n) => (r, p, n, 1),
        Command::Delete(r, p, d) => (r, p, String::new(), if d { 3 } else { 2 }),
        _ => unreachable!(),
    };
    if kind < 2 {
        let n = storage::safe_relative(&name)?;
        ensure!(n.components().count() == 1, "请输入单个文件名");
    }
    if kind != 0 {
        ensure!(
            !path.trim_end_matches(['\\', '/']).is_empty()
                && path.trim_end_matches(['\\', '/']) != ":"
                && !path.trim_end_matches(['\\', '/']).ends_with(':'),
            "不能修改或删除磁盘根目录"
        );
    }
    if remote {
        if kind == 0 {
            tasks::create_directory(wire.context("文件连接未就绪")?, path, name, stop).await?;
            return Ok(());
        }
        let req = match kind {
            0 => Req::DirCreate(FileDirCreate {
                path: join_remote(&path, &name),
            }),
            1 => {
                let parent = path
                    .rsplit_once(['\\', '/'])
                    .context("不能重命名远端根目录")?
                    .0;
                Req::Rename(FileRename {
                    new_name: join_remote(parent, &name),
                    path,
                })
            }
            2 => Req::RemoveFile(FileRemoveFile { path }),
            _ => Req::RemoveDir(FileRemoveDir {
                id: None,
                path,
                recursive: true,
            }),
        };
        tasks::operation(wire.context("文件连接未就绪")?, req, stop)
            .await
            .map(|_| ())
    } else {
        tokio::task::spawn_blocking(move || -> Result<()> {
            ensure!(!stop.is_cancelled(), "文件操作已取消，未执行");
            let p = PathBuf::from(path);
            let parent = storage::canonical_dir(if kind == 0 {
                &p
            } else {
                p.parent().context("不能操作磁盘根目录")?
            })?;
            let relative = if kind == 0 {
                storage::safe_relative(&name)?
            } else {
                storage::safe_relative(
                    p.file_name()
                        .and_then(|n| n.to_str())
                        .context("文件名无效")?,
                )?
            };
            let _locks = storage::parents(&parent, &relative, false)?;
            let target = parent.join(&relative);
            ensure!(!stop.is_cancelled(), "文件操作已取消，未执行");
            match kind {
                0 => std::fs::create_dir(target)?,
                1 => {
                    ensure!(!parent.join(&name).exists(), "目标名称已经存在");
                    ensure!(!stop.is_cancelled(), "文件操作已取消，未执行");
                    std::fs::rename(target, parent.join(name))?;
                }
                2 => std::fs::remove_file(target)?,
                _ => {
                    let _ = storage::scan(&target, &stop)?;
                    ensure!(!stop.is_cancelled(), "已取消");
                    std::fs::remove_dir_all(target)?;
                }
            }
            Ok(())
        })
        .await?
    }
}
pub(crate) fn join_remote(parent: &str, name: &str) -> String {
    let separator = if parent.contains('\\') || parent.as_bytes().get(1) == Some(&b':') {
        '\\'
    } else {
        '/'
    };
    format!(
        "{}{separator}{}",
        parent.trim_end_matches(['\\', '/']),
        name
    )
}
