use super::*;
use crate::account::client::AuthenticatedClient;
use crate::session::controller::ControllerConnection;

#[derive(Clone, Default)]
pub(crate) struct Snapshot {
    pub rules: Vec<(Rule, RuleStatus)>,
    pub enabled: bool,
    pub connected: bool,
    pub busy: bool,
    pub error: Option<String>,
}
pub(crate) enum Command {
    Save(Rule),
    Delete(u64),
    Enable(u64, bool),
    Probe(u64),
    Retry,
}
#[derive(Clone)]
pub(crate) struct Handle {
    state: Arc<Mutex<Snapshot>>,
    commands: mpsc::Sender<Command>,
    enabled: tokio::sync::watch::Sender<bool>,
    stop: CancellationToken,
}
impl Handle {
    pub(crate) fn send(&self, command: Command) -> Result<()> {
        self.commands
            .try_send(command)
            .context("上一个操作尚未完成")
    }
    pub(crate) fn set_enabled(&self, enabled: bool) {
        self.enabled.send_replace(enabled);
        lock(&self.state).enabled = enabled;
    }
    pub(crate) fn snapshot(&self) -> Snapshot {
        lock(&self.state).clone()
    }
}
struct Running {
    rule: Rule,
    stop: CancellationToken,
    probe: Arc<Notify>,
    status: Arc<Mutex<RuleStatus>>,
    task: tokio::task::JoinHandle<()>,
    counted_sent: u64,
    counted_received: u64,
    sampled: std::time::Instant,
    sent: u64,
    received: u64,
}
fn accumulate(running: &mut Running, totals: &mut HashMap<u64, (u64, u64)>) {
    let status = lock(&running.status);
    let total = totals.entry(running.rule.id).or_default();
    total.0 = total
        .0
        .saturating_add(status.sent.saturating_sub(running.counted_sent));
    total.1 = total
        .1
        .saturating_add(status.received.saturating_sub(running.counted_received));
    running.counted_sent = status.sent;
    running.counted_received = status.received;
}
fn services() -> &'static Mutex<HashMap<String, Handle>> {
    static SERVICES: std::sync::OnceLock<Mutex<HashMap<String, Handle>>> =
        std::sync::OnceLock::new();
    SERVICES.get_or_init(Mutex::default)
}
fn key(controller: &str, target: &str) -> String {
    format!("{controller}:{target}")
}
pub(crate) fn status(controller: &str, target: &str) -> Option<Snapshot> {
    lock(services())
        .get(&key(controller, target))
        .filter(|h| !h.stop.is_cancelled())
        .map(Handle::snapshot)
}

fn jobs() -> &'static Mutex<Vec<(CancellationToken, CancellationToken)>> {
    static JOBS: std::sync::OnceLock<Mutex<Vec<(CancellationToken, CancellationToken)>>> =
        std::sync::OnceLock::new();
    JOBS.get_or_init(Mutex::default)
}
pub(crate) async fn shutdown_all() {
    let jobs = std::mem::take(&mut *lock(jobs()));
    for (stop, _) in &jobs {
        stop.cancel();
    }
    lock(services()).clear();
    for (_, done) in jobs {
        done.cancelled().await;
    }
}
pub(crate) fn start(
    client: Arc<AuthenticatedClient>,
    device: crate::account::api::DeviceInfo,
    transport: crate::media::ConnectionMediaOptions,
) -> Handle {
    let mut services = lock(services());
    services.retain(|_, h| !h.stop.is_cancelled() && !h.commands.is_closed());
    let key = key(&client.device_id(), &device.device_id);
    if let Some(handle) = services.get(&key) {
        return handle.clone();
    }
    let state = Arc::new(Mutex::new(Snapshot::default()));
    let stop = client.ended().child_token();
    let (commands, receiver) = mpsc::channel(16);
    let (enabled, enabled_rx) = tokio::sync::watch::channel(false);
    let shared = Arc::clone(&state);
    let cancel = stop.clone();
    let done = CancellationToken::new();
    {
        let mut jobs = lock(jobs());
        jobs.retain(|(_, done)| !done.is_cancelled());
        jobs.push((stop.clone(), done.clone()));
    }
    tokio::spawn(async move {
        let _finish = done.drop_guard();
        if let Err(error) = run(
            client,
            device,
            transport,
            receiver,
            enabled_rx,
            cancel,
            Arc::clone(&shared),
        )
        .await
        {
            let mut s = lock(&shared);
            s.error = Some(format!("{error:#}"));
            s.busy = false;
            s.connected = false;
        }
    });
    let handle = Handle {
        state,
        commands,
        enabled,
        stop,
    };
    services.insert(key, handle.clone());
    handle
}

async fn connect(
    client: &AuthenticatedClient,
    device: &crate::account::api::DeviceInfo,
    transport: crate::media::ConnectionMediaOptions,
    stop: &CancellationToken,
) -> Result<ControllerConnection> {
    let list = tokio::select! {biased;_=stop.cancelled()=>anyhow::bail!("已取消连接"),result=client.list_devices()=>result?};
    let current = list.with_capability(&device.device_id, farwing_protocol::caps::PORTS)?;
    ensure!(
        current.platform.is_desktop()
            && current.controllable
            && current.controlled_support
            && current.is_connected(),
        "设备当前不可连接"
    );
    ensure!(
        current.device_id != client.device_id(),
        "不能连接本机虚拟设备"
    );
    let policy = client.feature_catalog().policy_for(current);
    ensure!(
        policy.supports(crate::account::feature_ability::Feature::PortMapping),
        "被控端版本不支持端口转发"
    );
    ControllerConnection::connect_mapping(client, current, policy, transport, stop).await
}

async fn run(
    client: Arc<AuthenticatedClient>,
    device: crate::account::api::DeviceInfo,
    network: crate::media::ConnectionMediaOptions,
    mut commands: mpsc::Receiver<Command>,
    mut enabled: tokio::sync::watch::Receiver<bool>,
    stop: CancellationToken,
    state: Arc<Mutex<Snapshot>>,
) -> Result<()> {
    let store = client.port_mapping_store(&device.device_id)?;
    let mut rules = store.load()?;
    let mut totals = HashMap::<u64, (u64, u64)>::new();
    let mut running: HashMap<u64, Running> = HashMap::new();
    let mut connection = None;
    let mut alive = None;
    let mut request_stop: Option<tokio::sync::oneshot::Sender<()>> = None;
    let mut pending: Option<tokio::task::JoinHandle<Result<ControllerConnection>>> = None;
    let mut attempt = stop.child_token();
    let mut active = false;
    let mut retry = false;
    let mut tick = tokio::time::interval(Duration::from_millis(200));
    loop {
        if !active || stop.is_cancelled() {
            for r in running.values() {
                r.stop.cancel();
            }
            for (_, mut r) in running.drain() {
                let _ = (&mut r.task).await;
                accumulate(&mut r, &mut totals);
            }
            attempt.cancel();
            if let Some(task) = pending.take() {
                if let Ok(Ok(controller)) = task.await {
                    let _ = controller.close().await;
                }
            }
            if let Some(tx) = request_stop.take() {
                let _: Result<(), ()> = tx.send(());
            }
            if let Some(task) = alive.take() {
                let _ = task.await;
            }
            connection = None;
            retry = false;
            lock(&state).busy = false;
            if stop.is_cancelled() {
                break;
            }
        }
        if retry && active && pending.is_none() && connection.is_none() {
            retry = false;
            attempt = stop.child_token();
            let cancel = attempt.clone();
            let client = Arc::clone(&client);
            let device = device.clone();
            lock(&state).busy = true;
            lock(&state).error = None;
            pending = Some(tokio::spawn(async move {
                let controller = connect(&client, &device, network, &cancel).await?;
                let ready = tokio::select! {
                    biased;
                    _=cancel.cancelled()=>Err(anyhow::anyhow!("已取消连接")),
                    result=controller.wait_port_mapping_ready()=>result,
                };
                if let Err(error) = ready {
                    let _ = controller.close().await;
                    return Err(error);
                }
                Ok(controller)
            }));
        }
        if pending.as_ref().is_some_and(|p| p.is_finished()) {
            match pending.take().unwrap().await {
                Ok(Ok(controller)) => {
                    let wire = controller.port_mapping_transport();
                    let (tx, rx) = tokio::sync::oneshot::channel();
                    request_stop = Some(tx);
                    alive = Some(tokio::spawn(controller.keep_alive(rx)));
                    connection = Some(wire);
                }
                Ok(Err(error)) => lock(&state).error = Some(format!("{error:#}")),
                Err(error) => lock(&state).error = Some(error.to_string()),
            }
            lock(&state).busy = false;
        }
        if alive
            .as_ref()
            .is_some_and(|t: &tokio::task::JoinHandle<Result<()>>| t.is_finished())
        {
            let result = alive.take().unwrap().await;
            lock(&state).error = Some(match result {
                Ok(Err(e)) => e.to_string(),
                Err(e) => e.to_string(),
                _ => "设备连接已关闭".into(),
            });
            connection = None;
            request_stop = None;
        }
        let obsolete: Vec<_> = running
            .iter()
            .filter(|(id, r)| {
                connection.is_none()
                    || !rules.iter().any(|d| {
                        d.id == **id
                            && d.enabled
                            && d.protocol == r.rule.protocol
                            && d.local_addr == r.rule.local_addr
                            && d.local_port == r.rule.local_port
                            && d.target == r.rule.target
                            && d.remote_port == r.rule.remote_port
                    })
            })
            .map(|(id, _)| *id)
            .collect();
        for id in &obsolete {
            running[id].stop.cancel();
        }
        for id in obsolete {
            if let Some(mut r) = running.remove(&id) {
                let _ = (&mut r.task).await;
                accumulate(&mut r, &mut totals);
            }
        }
        if let Some(wire) = &connection {
            for rule in rules.iter().filter(|r| r.enabled) {
                if running.contains_key(&rule.id) {
                    continue;
                }
                let cancel = stop.child_token();
                let probe = Arc::new(Notify::new());
                let status = Arc::new(Mutex::new(RuleStatus::default()));
                let task = tokio::spawn(super::run_rule(
                    rule.clone(),
                    Arc::clone(wire),
                    cancel.clone(),
                    Arc::clone(&status),
                    Arc::clone(&probe),
                ));
                running.insert(
                    rule.id,
                    Running {
                        rule: rule.clone(),
                        stop: cancel,
                        probe,
                        status,
                        task,
                        counted_sent: 0,
                        counted_received: 0,
                        sampled: std::time::Instant::now(),
                        sent: 0,
                        received: 0,
                    },
                );
            }
        }
        for r in running.values_mut() {
            accumulate(r, &mut totals);
            let elapsed = r.sampled.elapsed().as_secs_f64();
            if elapsed >= 1.0 {
                let mut status = lock(&r.status);
                status.send_rate = status.sent.saturating_sub(r.sent) as f64 / elapsed;
                status.receive_rate = status.received.saturating_sub(r.received) as f64 / elapsed;
                r.sent = status.sent;
                r.received = status.received;
                r.sampled = std::time::Instant::now();
            }
        }
        {
            let mut s = lock(&state);
            s.enabled = active;
            s.connected = connection.is_some();
            totals.retain(|id, _| rules.iter().any(|r| r.id == *id));
            s.rules = rules
                .iter()
                .map(|r| {
                    let mut status = running
                        .get(&r.id)
                        .map(|t| lock(&t.status).clone())
                        .unwrap_or_default();
                    let total = totals.get(&r.id).copied().unwrap_or_default();
                    status.total_sent = total.0;
                    status.total_received = total.1;
                    (r.clone(), status)
                })
                .collect();
        }

        tokio::select! {
            biased;
            _=stop.cancelled()=>{},
            changed=enabled.changed()=>{
                if changed.is_err(){stop.cancel();continue;}
                active=*enabled.borrow_and_update();
                retry=active && connection.is_none();
                if !active {lock(&state).error=None;}
            },
            _=tick.tick()=>{},
            command=commands.recv()=>{
                let Some(command)=command else {stop.cancel();continue;};
                if matches!(command,Command::Retry) {if active && connection.is_none(){retry=true;}continue;}
                if let Command::Probe(id)=command {if let Some(r)=running.get(&id){r.probe.notify_one();}continue;}
                let mut next=rules.clone();
                let result=(||->Result<()> {
                    match command {
                        Command::Save(mut rule)=>{
                            rule.validate()?;
                            if let Some(index)=next.iter().position(|r|r.id==rule.id) {
                                let old=&next[index];
                                if old.enabled && (old.protocol!=rule.protocol || old.target!=rule.target || old.remote_port!=rule.remote_port || old.local_addr!=rule.local_addr || old.local_port!=rule.local_port) {rule.id=new_id();}
                                next[index]=rule;
                            } else {ensure!(next.len()<256,"规则过多");next.push(rule);}
                        },
                        Command::Delete(id)=>next.retain(|r|r.id!=id),
                        Command::Enable(id,value)=>{next.iter_mut().find(|r|r.id==id).context("规则不存在")?.enabled=value;},
                        Command::Retry|Command::Probe(_)=>{},
                    }
                    store.save(&next)?;rules=next;Ok(())
                })();
                lock(&state).error=result.err().map(|e|e.to_string());
            }
        }
    }
    let mut s = lock(&state);
    s.enabled = false;
    s.connected = false;
    s.busy = false;
    Ok(())
}
pub(crate) fn new_id() -> u64 {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;
    NEXT.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |old| {
        Some(old.saturating_add(1).max(now))
    })
    .unwrap()
    .saturating_add(1)
    .max(now)
}
