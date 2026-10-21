//! One explicit diagnostic job per owned device. Uses a data-only shared lease.
use super::{Controller, Snapshot, lock};
use crate::{
    account::{api::DeviceInfo, client::AuthenticatedClient},
    media::ConnectionMediaOptions,
    session::controller::ControllerConnection,
};
use anyhow::{Context, Result, ensure};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex, OnceLock},
    time::Duration,
};
use tokio_util::sync::CancellationToken;
struct Job {
    view: Mutex<Snapshot>,
    cancel: CancellationToken,
    done: CancellationToken,
}
fn jobs() -> &'static Mutex<HashMap<String, Arc<Job>>> {
    static JOBS: OnceLock<Mutex<HashMap<String, Arc<Job>>>> = OnceLock::new();
    JOBS.get_or_init(Mutex::default)
}
fn key(controller: &str, target: &str) -> String {
    format!("{controller}:{target}")
}
pub(crate) fn snapshot(controller: &str, target: &str) -> Option<Snapshot> {
    lock(jobs())
        .get(&key(controller, target))
        .map(|job| lock(&job.view).clone())
}
pub(crate) fn cancel(controller: &str, target: &str) {
    if let Some(job) = lock(jobs()).get(&key(controller, target)) {
        job.cancel.cancel();
        lock(&job.view).progress.stage = "正在取消…".into();
    }
}
pub(crate) fn start(
    client: Arc<AuthenticatedClient>,
    device: DeviceInfo,
    options: ConnectionMediaOptions,
) {
    let key = key(&client.device_id(), &device.device_id);
    let mut jobs = lock(jobs());
    if jobs.get(&key).is_some_and(|job| !job.done.is_cancelled()) {
        return;
    }
    let job = Arc::new(Job {
        view: Mutex::new(Snapshot {
            busy: true,
            progress: super::bundle::Progress {
                stage: "正在连接设备…".into(),
                completed: 0,
                total: 0,
            },
            ..Default::default()
        }),
        cancel: client.ended().child_token(),
        done: CancellationToken::new(),
    });
    jobs.insert(key, job.clone());
    tokio::spawn(async move {
        let _done = job.done.clone().drop_guard();
        let result = run(&client, &device, options, &job).await;
        let mut view = lock(&job.view);
        match result {
            Ok(value) => *view = value,
            Err(error) => {
                view.cancelled = job.cancel.is_cancelled();
                if view.cancelled {
                    view.error = None;
                    view.progress.stage = "已取消获取".into();
                } else {
                    // Keep the error chain in diagnostics, not in the product label.
                    tracing::warn!(error = %format!("{error:#}"), "remote diagnostic request failed");
                    view.error = Some(error.to_string());
                }
                view.path = None;
            }
        }
        view.busy = false;
    });
}
pub(crate) async fn shutdown_all() {
    let jobs = std::mem::take(&mut *lock(jobs()));
    for job in jobs.values() {
        job.cancel.cancel()
    }
    for job in jobs.values() {
        job.done.cancelled().await
    }
}
async fn run(
    client: &AuthenticatedClient,
    device: &DeviceInfo,
    options: ConnectionMediaOptions,
    job: &Job,
) -> Result<Snapshot> {
    let list = tokio::select! {_=job.cancel.cancelled()=>anyhow::bail!("已取消获取远端诊断包"),list=client.list_devices()=>list.context("暂时无法读取设备状态")?};
    let target = list.with_capability(&device.device_id, farwing_protocol::caps::DIAGNOSTICS)?;
    target
        .validated_device_id()
        .context("设备信息无效")?;
    ensure!(
        target.platform == farwing_protocol::Platform::Windows
            && target.device_id != client.device_id()
            && target.is_connected()
            && target.controllable
            && target.controlled_support,
        "设备当前不可获取诊断包"
    );
    let policy = client.feature_catalog().policy_for(target);
    let connection =
        ControllerConnection::connect_files(client, target, policy, options, &job.cancel)
            .await
            .context("连接设备失败")?;
    lock(&job.view).progress.stage = "等待远端响应…".into();
    let wire = connection.stream_control_handle().diagnostics().clone();
    let mut started = false;
    let result = collect(&wire, job, &mut started).await;
    if result.is_err() && started {
        wire.cancel();
        // Let the per-request receiver delete its partial before releasing our
        // lease. This never closes another viewer or file-transfer attachment.
        while wire.snapshot().busy {
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }
    let closed = connection.close().await;
    match result {
        Ok(snapshot) => {
            if let Err(error) = closed {
                tracing::debug!(%error,"diagnostic lease release after completed download");
            }
            Ok(snapshot)
        }
        Err(error) => Err(error),
    }
}
async fn collect(wire: &Controller, job: &Job, started: &mut bool) -> Result<Snapshot> {
    tokio::select! {
        _=job.cancel.cancelled()=>anyhow::bail!("已取消获取远端诊断包"),
        ready=tokio::time::timeout(Duration::from_secs(15),async {
            loop {
                ensure!(!wire.is_closed(), "诊断连接已关闭");
                let view=wire.snapshot();
                if view.supported {ensure!(view.allowed,"远端未允许账号内文件访问");return Ok::<_,anyhow::Error>(());}
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })=>ready.map_err(|_| anyhow::anyhow!("远端未响应诊断请求"))??,
    }
    wire.start().context("无法开始获取诊断包")?;
    *started = true;
    loop {
        let snapshot = wire.snapshot();
        if !snapshot.busy {
            if let Some(error) = &snapshot.error {
                return Err(anyhow::anyhow!(error.clone()).context("诊断包获取未完成"));
            }
            ensure!(snapshot.path.is_some(), "远端诊断包未完成");
            return Ok(snapshot);
        }
        *lock(&job.view) = snapshot;
        tokio::select! {_=job.cancel.cancelled()=>anyhow::bail!("已取消获取远端诊断包"),_=tokio::time::sleep(Duration::from_millis(100))=>{}}
    }
}
