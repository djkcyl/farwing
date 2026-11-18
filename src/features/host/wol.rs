//! Optional account-owned LAN registration and WoL relay, independent of media threads.
pub(crate) mod packet;
pub(crate) mod setup;
use crate::{platform::wol as network, session::host_client::HostClient};
use anyhow::{Result, ensure};
use packet::{LanInfo, Target};
use serde::{Deserialize, Serialize};
use std::{
    collections::VecDeque,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};
use tokio::sync::{Notify, mpsc};
use tokio_util::sync::CancellationToken;
#[derive(Clone, Default, Serialize, Deserialize)]
pub(crate) struct Status {
    pub message: String,
    pub error: Option<String>,
    pub interface_name: Option<String>,
    pub network: Option<LanInfo>,
    pub support_wol: Option<bool>,
    pub registered_at: Option<i64>,
    pub last_sent_at: Option<i64>,
    pub packets_sent: u64,
    pub skipped: u64,
    pub reporting: bool,
}
#[derive(Default)]
struct Control {
    revision: AtomicU64,
    online: AtomicBool,
    force: AtomicBool,
    changed: Notify,
    binding: Mutex<Option<(u64, mpsc::Sender<Queued>)>>,
    next: AtomicU64,
    network: Arc<network::Changes>,
}
#[derive(Clone, Default)]
pub(crate) struct Handle(Arc<Control>);
struct Queued {
    target: Target,
    revision: u64,
    room: CancellationToken,
    received: Instant,
    network_revision: u64,
}
impl Handle {
    pub fn revision(&self) -> u64 {
        self.0.revision.load(Ordering::Acquire)
    }
    pub fn change(&self) {
        self.0.revision.fetch_add(1, Ordering::AcqRel);
        self.0.changed.notify_one();
    }
    pub fn refresh(&self) {
        self.0.force.store(true, Ordering::Release);
        self.0.changed.notify_one();
    }
    pub fn online(&self, value: bool) {
        if self.0.online.swap(value, Ordering::AcqRel) != value {
            if value {
                self.0.force.store(true, Ordering::Release);
            }
            self.change();
        }
    }
    pub fn push(&self, push: &serde_json::Value, room: &CancellationToken) -> bool {
        let target = match Target::parse(push) {
            Ok(Some(t)) => t,
            Ok(None) => return false,
            Err(_) => {
                tracing::warn!("invalid WoL push discarded");
                return true;
            }
        };
        if !self.0.online.load(Ordering::Acquire) || room.is_cancelled() {
            return true;
        }
        if let Some((_, sender)) = &*super::lock(&self.0.binding) {
            if sender
                .try_send(Queued {
                    target,
                    revision: self.revision(),
                    room: room.clone(),
                    received: Instant::now(),
                    network_revision: self.0.network.revision.load(Ordering::Acquire),
                })
                .is_err()
            {
                tracing::warn!("WoL request queue full; request discarded");
            }
        }
        true
    }
}
pub(crate) struct Running {
    stop: CancellationToken,
    task: tokio::task::JoinHandle<()>,
    handle: Handle,
    binding: u64,
}
impl Running {
    pub fn start(client: HostClient) -> Option<Self> {
        if client.is_guest() {
            return None;
        }
        let handle = client.host.wol.clone();
        let binding = handle.0.next.fetch_add(1, Ordering::AcqRel);
        let (tx, rx) = mpsc::channel(8);
        *super::lock(&handle.0.binding) = Some((binding, tx));
        handle.change();
        let stop = CancellationToken::new();
        let stopped = stop.clone();
        let task = tokio::spawn(async move {
            run(client, rx, stopped).await;
        });
        Some(Self {
            stop,
            task,
            handle,
            binding,
        })
    }
}
impl Drop for Running {
    fn drop(&mut self) {
        self.stop.cancel();
        self.task.abort();
        let mut binding = super::lock(&self.handle.0.binding);
        if binding.as_ref().is_some_and(|(id, _)| *id == self.binding) {
            *binding = None;
            self.handle.online(false);
        }
    }
}
fn allowed(client: &HostClient, revision: u64, stop: &CancellationToken) -> bool {
    !stop.is_cancelled()
        && client.is_active()
        && client.host.wol_registration_permitted()
        && client.host.wol.revision() == revision
        && client.host.wol.0.online.load(Ordering::Acquire)
}
async fn inventory() -> Result<Vec<network::Interface>> {
    tokio::task::spawn_blocking(network::inventory)
        .await
        .map_err(|_| anyhow::anyhow!("网络读取任务中断"))?
}
struct ReportTask {
    revision: u64,
    network_revision: u64,
    info: LanInfo,
    task: tokio::task::JoinHandle<Result<bool>>,
}
impl Drop for ReportTask {
    fn drop(&mut self) {
        self.task.abort();
    }
}
async fn run(client: HostClient, mut requests: mpsc::Receiver<Queued>, stop: CancellationToken) {
    let handle = client.host.wol.clone();
    let ended = client.ended();
    let changes = handle.0.network.clone();
    let mut watcher: Option<network::Watcher> = None;
    let mut dirty = true;
    let mut last_attempt: Option<LanInfo> = None;
    let mut report: Option<ReportTask> = None;
    let mut status = Status::default();
    let mut recent: VecDeque<(Target, Instant)> = VecDeque::new();
    loop {
        if stop.is_cancelled() || ended.is_cancelled() {
            break;
        }
        if dirty {
            dirty = false;
            let revision = handle.revision();
            let force = handle.0.force.swap(false, Ordering::AcqRel);
            if !allowed(&client, revision, &stop) {
                report = None;
                watcher = None;
                last_attempt = None;
                recent.clear();
                while requests.try_recv().is_ok() {}
                status = Status {
                    message: if !client.host.wol_allowed() && !client.host.wol_setup.enabled() {
                        "远程开机与唤醒协助均未开启"
                    } else if !client.host.allowed() {
                        "需先开启允许被控"
                    } else {
                        "等待账号上线"
                    }
                    .into(),
                    ..Default::default()
                };
                client.host.wol_status(revision, status.clone());
            } else {
                let result = async {
                    if watcher.is_none() {
                        watcher = Some(network::Watcher::new(changes.clone())?);
                    }
                    let network_revision = changes.revision.load(Ordering::Acquire);
                    let list = inventory().await?;
                    ensure!(
                        allowed(&client, revision, &stop)
                            && network_revision == changes.revision.load(Ordering::Acquire),
                        "网络或唤醒许可已变化"
                    );
                    let primary = list
                        .iter()
                        .find(|i| i.default_metric.is_some())
                        .ok_or_else(|| anyhow::anyhow!("没有可登记的物理IPv4默认网络"))?;
                    let info = primary.registration();
                    status.interface_name = Some(primary.name.clone());
                    status.network = Some(info.clone());
                    if !force
                        && report.as_ref().is_some_and(|r| {
                            r.info == info
                                && r.revision == revision
                                && r.network_revision == network_revision
                        })
                    {
                        return Ok(());
                    }
                    report = None;
                    if !force && last_attempt.as_ref() == Some(&info) {
                        status.reporting = false;
                        return Ok(());
                    }
                    status.reporting = true;
                    status.support_wol = None;
                    status.message = "正在登记局域网信息…".into();
                    status.error = None;
                    client.host.wol_status(revision, status.clone());
                    let reporting_client = client.clone();
                    let payload = info.clone();
                    report = Some(ReportTask {
                        revision,
                        network_revision,
                        info,
                        task: tokio::spawn(async move { reporting_client.wol_info(payload).await }),
                    });
                    Ok::<(), anyhow::Error>(())
                }
                .await;
                if let Err(e) = result {
                    report = None;
                    last_attempt = None;
                    status.network = None;
                    status.interface_name = None;
                    status.support_wol = None;
                    status.registered_at = None;
                    status.reporting = false;
                    status.message = "局域网登记未完成".into();
                    status.error = Some(e.to_string());
                }
                client.host.wol_status(revision, status.clone());
            }
        }
        if dirty {
            continue;
        }
        tokio::select! {biased;
            _=stop.cancelled()=>break,
            _=ended.cancelled()=>break,
            _=handle.0.changed.notified()=>dirty=true,
            _=changes.notify.notified()=>{
                tokio::select!{_=stop.cancelled()=>break,_=ended.cancelled()=>break,_=tokio::time::sleep(Duration::from_millis(300))=>{}}
                dirty=true;
            },
            result=async {match &mut report {Some(r)=>(&mut r.task).await,None=>std::future::pending().await}}=>{
                let completed=report.take().unwrap();
                if allowed(&client,completed.revision,&stop)&&completed.network_revision==changes.revision.load(Ordering::Acquire) {
                    last_attempt=Some(completed.info.clone());status.reporting=false;
                    match result.map_err(|_|anyhow::anyhow!("网络登记任务中断")).and_then(|r|r) {
                        Ok(supported)=>{status.support_wol=Some(supported);status.registered_at=Some(chrono::Utc::now().timestamp());status.message="局域网信息已登记".into();status.error=None;},
                        Err(error)=>{status.message="局域网登记未完成".into();status.error=Some(error.to_string());}
                    }
                    client.host.wol_status(completed.revision,status.clone());
                } else {dirty=true;}
            },
            request=requests.recv()=>{
                let Some(request)=request else{break;};
                if !client.host.wol_permitted() || !allowed(&client,request.revision,&stop)||request.room.is_cancelled()||request.network_revision!=changes.revision.load(Ordering::Acquire){continue;}
                if request.received.elapsed()>Duration::from_secs(5){status.skipped+=1;status.error=Some("唤醒请求已过期".into());client.host.wol_status(request.revision,status.clone());continue;}
                recent.retain(|(_,at)|at.elapsed()<Duration::from_secs(2));
                if recent.iter().any(|(target,_)|*target==request.target){status.skipped+=1;client.host.wol_status(request.revision,status.clone());continue;}
                if recent.len()==64{recent.pop_front();}recent.push_back((request.target,Instant::now()));
                let watched=watcher.is_some();
                let result=async {
                    ensure!(watched,"网络监听尚未就绪");let network_revision=request.network_revision;
                    let interfaces=inventory().await?;
                    let interface=interfaces.into_iter().find(|i|request.target.matches(i.ip,i.prefix)).ok_or_else(||anyhow::anyhow!("目标网段与本机物理网络不匹配"))?;
                    let packet=request.target.packet();let destinations=request.target.destinations();
                    let sender=client.clone();let stopped=stop.clone();let network_changes=changes.clone();let room=request.room.clone();let revision=request.revision;
                    tokio::task::spawn_blocking(move||network::send(&interface,&packet,&destinations,||{
                        sender.host.wol_permitted() && allowed(&sender,revision,&stopped)&&!room.is_cancelled()&&network_changes.revision.load(Ordering::Acquire)==network_revision
                    })).await.map_err(|_|anyhow::anyhow!("唤醒发送任务中断"))?
                }.await;
                match result {Ok(sent)=>{status.packets_sent+=u64::from(sent.sent);status.last_sent_at=Some(chrono::Utc::now().timestamp());status.message=format!("已发送 {}/{} 个唤醒报文",sent.sent,sent.attempted);status.error=None;tracing::info!(sent=sent.sent,attempted=sent.attempted,"WoL relay packets submitted");},Err(e)=>{status.skipped+=1;status.error=Some(e.to_string());tracing::warn!(%e,"WoL relay request not completed");}}
                client.host.wol_status(request.revision,status.clone());
            }
        }
    }
}
